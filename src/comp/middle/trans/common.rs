/**
   Code that is useful in various trans modules.

*/

import ctypes::unsigned;
import vec::to_ptr;
import std::map::hashmap;
import syntax::ast;
import driver::session;
import session::session;
import middle::{resolve, ty};
import back::{link, abi, upcall};
import util::common::*;
import syntax::codemap::span;
import lib::llvm::{llvm, target_data, type_names, associate_type,
                   name_has_type};
import lib::llvm::{ModuleRef, ValueRef, TypeRef, BasicBlockRef, BuilderRef};
import lib::llvm::{True, False, Bool};
import metadata::csearch;
import ast_map::path;

// FIXME: These should probably be pulled in here too.
import base::{type_of_fn, drop_ty};

type namegen = fn@(str) -> str;
fn new_namegen() -> namegen {
    let i = @mutable 0;
    ret fn@(prefix: str) -> str { *i += 1; prefix + int::str(*i) };
}

type derived_tydesc_info = {lltydesc: ValueRef, escapes: bool};

enum tydesc_kind {
    tk_static, // Static (monomorphic) type descriptor
    tk_param, // Type parameter.
    tk_derived, // Derived from a typaram or another derived tydesc.
}

type tydesc_info =
    {ty: ty::t,
     tydesc: ValueRef,
     size: ValueRef,
     align: ValueRef,
     mutable take_glue: option<ValueRef>,
     mutable drop_glue: option<ValueRef>,
     mutable free_glue: option<ValueRef>,
     ty_params: [uint]};

/*
 * A note on nomenclature of linking: "upcall", "extern" and "native".
 *
 * An "extern" is an LLVM symbol we wind up emitting an undefined external
 * reference to. This means "we don't have the thing in this compilation unit,
 * please make sure you link it in at runtime". This could be a reference to
 * C code found in a C library, or rust code found in a rust crate.
 *
 * A "native" is an extern that references C code. Called with cdecl.
 *
 * An upcall is a native call generated by the compiler (not corresponding to
 * any user-written call in the code) into librustrt, to perform some helper
 * task such as bringing a task to life, allocating memory, etc.
 *
 */
type stats =
    {mutable n_static_tydescs: uint,
     mutable n_derived_tydescs: uint,
     mutable n_glues_created: uint,
     mutable n_null_glues: uint,
     mutable n_real_glues: uint,
     fn_times: @mutable [{ident: str, time: int}]};

resource BuilderRef_res(B: BuilderRef) { llvm::LLVMDisposeBuilder(B); }

// Crate context.  Every crate we compile has one of these.
type crate_ctxt =
    // A mapping from the def_id of each item in this crate to the address
    // of the first instruction of the item's definition in the executable
    // we're generating.
    // TODO: hashmap<tup(tag_id,subtys), @tag_info>
    {sess: session::session,
     llmod: ModuleRef,
     td: target_data,
     tn: type_names,
     externs: hashmap<str, ValueRef>,
     intrinsics: hashmap<str, ValueRef>,
     item_ids: hashmap<ast::node_id, ValueRef>,
     ast_map: ast_map::map,
     exp_map: resolve::exp_map,
     item_symbols: hashmap<ast::node_id, str>,
     mutable main_fn: option<ValueRef>,
     link_meta: link::link_meta,
     enum_sizes: hashmap<ty::t, uint>,
     discrims: hashmap<ast::def_id, ValueRef>,
     discrim_symbols: hashmap<ast::node_id, str>,
     consts: hashmap<ast::node_id, ValueRef>,
     tydescs: hashmap<ty::t, @tydesc_info>,
     dicts: hashmap<dict_id, ValueRef>,
     monomorphized: hashmap<mono_id, {llfn: ValueRef, fty: ty::t}>,
     module_data: hashmap<str, ValueRef>,
     lltypes: hashmap<ty::t, TypeRef>,
     names: namegen,
     sha: std::sha1::sha1,
     type_sha1s: hashmap<ty::t, str>,
     type_short_names: hashmap<ty::t, str>,
     tcx: ty::ctxt,
     mutbl_map: mutbl::mutbl_map,
     copy_map: alias::copy_map,
     last_uses: last_use::last_uses,
     impl_map: resolve::impl_map,
     method_map: typeck::method_map,
     dict_map: typeck::dict_map,
     stats: stats,
     upcalls: @upcall::upcalls,
     tydesc_type: TypeRef,
     int_type: TypeRef,
     float_type: TypeRef,
     task_type: TypeRef,
     opaque_vec_type: TypeRef,
     builder: BuilderRef_res,
     shape_cx: shape::ctxt,
     crate_map: ValueRef,
     dbg_cx: option<@debuginfo::debug_ctxt>,
     mutable do_not_commit_warning_issued: bool};

// Types used for llself.
type val_self_pair = {v: ValueRef, t: ty::t};

enum local_val { local_mem(ValueRef), local_imm(ValueRef), }

type fn_ty_param = {desc: ValueRef, dicts: option<[ValueRef]>};

type param_substs = {tys: [ty::t],
                     dicts: option<typeck::dict_res>,
                     bounds: @[ty::param_bounds]};

// Function context.  Every LLVM function we create will have one of
// these.
type fn_ctxt = {
    // The ValueRef returned from a call to llvm::LLVMAddFunction; the
    // address of the first instruction in the sequence of
    // instructions for this function that will go in the .text
    // section of the executable we're generating.
    llfn: ValueRef,

    // The two implicit arguments that arrive in the function we're creating.
    // For instance, foo(int, int) is really foo(ret*, env*, int, int).
    llenv: ValueRef,
    llretptr: ValueRef,

    // These elements: "hoisted basic blocks" containing
    // administrative activities that have to happen in only one place in
    // the function, due to LLVM's quirks.
    // A block for all the function's static allocas, so that LLVM
    // will coalesce them into a single alloca call.
    mutable llstaticallocas: BasicBlockRef,
    // A block containing code that copies incoming arguments to space
    // already allocated by code in one of the llallocas blocks.
    // (LLVM requires that arguments be copied to local allocas before
    // allowing most any operation to be performed on them.)
    mutable llloadenv: BasicBlockRef,
    // The first and last block containing derived tydescs received from the
    // runtime. See description of derived_tydescs, below.
    mutable llderivedtydescs_first: BasicBlockRef,
    mutable llderivedtydescs: BasicBlockRef,
    // A block for all of the dynamically sized allocas.  This must be
    // after llderivedtydescs, because these sometimes depend on
    // information computed from derived tydescs.
    mutable lldynamicallocas: BasicBlockRef,
    mutable llreturn: BasicBlockRef,
    // The token used to clear the dynamic allocas at the end of this frame.
    mutable llobstacktoken: option<ValueRef>,
    // The 'self' value currently in use in this function, if there
    // is one.
    mutable llself: option<val_self_pair>,
    // The a value alloca'd for calls to upcalls.rust_personality. Used when
    // outputting the resume instruction.
    mutable personality: option<ValueRef>,

    // Maps arguments to allocas created for them in llallocas.
    llargs: hashmap<ast::node_id, local_val>,
    // Maps the def_ids for local variables to the allocas created for
    // them in llallocas.
    lllocals: hashmap<ast::node_id, local_val>,
    // Same as above, but for closure upvars
    llupvars: hashmap<ast::node_id, ValueRef>,

    // A vector of incoming type descriptors and their associated iface dicts.
    mutable lltyparams: [fn_ty_param],

    // Derived tydescs are tydescs created at runtime, for types that
    // involve type parameters inside type constructors.  For example,
    // suppose a function parameterized by T creates a vector of type
    // [T].  The function doesn't know what T is until runtime, and
    // the function's caller knows T but doesn't know that a vector is
    // involved.  So a tydesc for [T] can't be created until runtime,
    // when information about both "[T]" and "T" are available.  When
    // such a tydesc is created, we cache it in the derived_tydescs
    // table for the next time that such a tydesc is needed.
    derived_tydescs: hashmap<ty::t, derived_tydesc_info>,

    // The node_id of the function, or -1 if it doesn't correspond to
    // a user-defined function.
    id: ast::node_id,

    // If this function is being monomorphized, this contains the type
    // substitutions used.
    param_substs: option<param_substs>,

    // The source span and nesting context where this function comes from, for
    // error reporting and symbol generation.
    span: option<span>,
    path: path,

    // This function's enclosing crate context.
    ccx: @crate_ctxt
};

fn warn_not_to_commit(ccx: @crate_ctxt, msg: str) {
    if !ccx.do_not_commit_warning_issued {
        ccx.do_not_commit_warning_issued = true;
        ccx.sess.warn(msg + " -- do not commit like this!");
    }
}

enum cleanup {
    clean(fn@(@block_ctxt) -> @block_ctxt),
    clean_temp(ValueRef, fn@(@block_ctxt) -> @block_ctxt),
}

// Used to remember and reuse existing cleanup paths
// target: none means the path ends in an resume instruction
type cleanup_path = {target: option<BasicBlockRef>,
                     dest: BasicBlockRef};

fn scope_clean_changed(cx: @block_ctxt) {
    cx.cleanup_paths = [];
    cx.landing_pad = none;
}

fn add_clean(cx: @block_ctxt, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(bcx_tcx(cx), ty) { ret; }
    let scope_cx = find_scope_cx(cx);
    scope_cx.cleanups += [clean(bind drop_ty(_, val, ty))];
    scope_clean_changed(scope_cx);
}
fn add_clean_temp(cx: @block_ctxt, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(bcx_tcx(cx), ty) { ret; }
    fn do_drop(bcx: @block_ctxt, val: ValueRef, ty: ty::t) ->
       @block_ctxt {
        if ty::type_is_immediate(ty) {
            ret base::drop_ty_immediate(bcx, val, ty);
        } else {
            ret drop_ty(bcx, val, ty);
        }
    }
    let scope_cx = find_scope_cx(cx);
    scope_cx.cleanups +=
        [clean_temp(val, bind do_drop(_, val, ty))];
    scope_clean_changed(scope_cx);
}
fn add_clean_temp_mem(cx: @block_ctxt, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(bcx_tcx(cx), ty) { ret; }
    let scope_cx = find_scope_cx(cx);
    scope_cx.cleanups += [clean_temp(val, bind drop_ty(_, val, ty))];
    scope_clean_changed(scope_cx);
}
fn add_clean_free(cx: @block_ctxt, ptr: ValueRef, shared: bool) {
    let scope_cx = find_scope_cx(cx);
    let free_fn = if shared { bind base::trans_shared_free(_, ptr) }
                  else { bind base::trans_free(_, ptr) };
    scope_cx.cleanups += [clean_temp(ptr, free_fn)];
    scope_clean_changed(scope_cx);
}

// Note that this only works for temporaries. We should, at some point, move
// to a system where we can also cancel the cleanup on local variables, but
// this will be more involved. For now, we simply zero out the local, and the
// drop glue checks whether it is zero.
fn revoke_clean(cx: @block_ctxt, val: ValueRef) {
    let sc_cx = find_scope_cx(cx);
    let found = -1;
    let i = 0;
    for c: cleanup in sc_cx.cleanups {
        alt c {
          clean_temp(v, _) {
            if v as uint == val as uint { found = i; break; }
          }
          _ { }
        }
        i += 1;
    }
    // The value does not have a cleanup associated with it.
    if found == -1 { ret; }
    // We found the cleanup and remove it
    sc_cx.cleanups =
        vec::slice(sc_cx.cleanups, 0u, found as uint) +
            vec::slice(sc_cx.cleanups, (found as uint) + 1u,
                            sc_cx.cleanups.len());
    scope_clean_changed(sc_cx);
    ret;
}

fn get_res_dtor(ccx: @crate_ctxt, did: ast::def_id, inner_t: ty::t)
   -> ValueRef {
    if did.crate == ast::local_crate {
        alt ccx.item_ids.find(did.node) {
          some(x) { ret x; }
          _ { ccx.tcx.sess.bug("get_res_dtor: can't find resource dtor!"); }
        }
    }

    let param_bounds = ty::lookup_item_type(ccx.tcx, did).bounds;
    let nil_res = ty::mk_nil(ccx.tcx);
    let fn_mode = ast::expl(ast::by_ref);
    let f_t = type_of_fn(ccx, [{mode: fn_mode, ty: inner_t}],
                         nil_res, *param_bounds);
    ret base::get_extern_const(ccx.externs, ccx.llmod,
                                csearch::get_symbol(ccx.sess.cstore,
                                                    did), f_t);
}

enum block_kind {
    // A scope at the end of which temporary values created inside of it are
    // cleaned up. May correspond to an actual block in the language, but also
    // to an implicit scope, for example, calls introduce an implicit scope in
    // which the arguments are evaluated and cleaned up.
    SCOPE_BLOCK,
    // A basic block created from the body of a loop.  Contains pointers to
    // which block to jump to in the case of "continue" or "break".
    LOOP_SCOPE_BLOCK(option<@block_ctxt>, @block_ctxt),
    // A non-scope block is a basic block created as a translation artifact
    // from translating code that expresses conditional logic rather than by
    // explicit { ... } block structure in the source language.  It's called a
    // non-scope block because it doesn't introduce a new variable scope.
    NON_SCOPE_BLOCK
}

// Basic block context.  We create a block context for each basic block
// (single-entry, single-exit sequence of instructions) we generate from Rust
// code.  Each basic block we generate is attached to a function, typically
// with many basic blocks per function.  All the basic blocks attached to a
// function are organized as a directed graph.
type block_ctxt =
    // The BasicBlockRef returned from a call to
    // llvm::LLVMAppendBasicBlock(llfn, name), which adds a basic
    // block to the function pointed to by llfn.  We insert
    // instructions into that block by way of this block context.
    // The block pointing to this one in the function's digraph.
    // The 'kind' of basic block this is.
    // A list of functions that run at the end of translating this
    // block, cleaning up any variables that were introduced in the
    // block and need to go out of scope at the end of it.
    // The source span where this block comes from, for error
    // reporting. FIXME this is not currently reliable
    // The function context for the function to which this block is
    // attached.
    {llbb: BasicBlockRef,
     mutable terminated: bool,
     mutable unreachable: bool,
     parent: block_parent,
     kind: block_kind,
     // FIXME the next five fields should probably only appear in scope blocks
     mutable cleanups: [cleanup],
     mutable cleanup_paths: [cleanup_path],
     mutable landing_pad: option<BasicBlockRef>,
     block_span: option<span>,
     fcx: @fn_ctxt};

// FIXME: we should be able to use option<@block_parent> here but
// the infinite-enum check in rustboot gets upset.
enum block_parent { parent_none, parent_some(@block_ctxt), }

type result = {bcx: @block_ctxt, val: ValueRef};
type result_t = {bcx: @block_ctxt, val: ValueRef, ty: ty::t};

fn rslt(bcx: @block_ctxt, val: ValueRef) -> result {
    {bcx: bcx, val: val}
}

fn ty_str(tn: type_names, t: TypeRef) -> str {
    ret lib::llvm::type_to_str(tn, t);
}

fn val_ty(&&v: ValueRef) -> TypeRef { ret llvm::LLVMTypeOf(v); }

fn val_str(tn: type_names, v: ValueRef) -> str { ret ty_str(tn, val_ty(v)); }

// Returns the nth element of the given LLVM structure type.
fn struct_elt(llstructty: TypeRef, n: uint) -> TypeRef unsafe {
    let elt_count = llvm::LLVMCountStructElementTypes(llstructty) as uint;
    assert (n < elt_count);
    let elt_tys = vec::init_elt(elt_count, T_nil());
    llvm::LLVMGetStructElementTypes(llstructty, to_ptr(elt_tys));
    ret llvm::LLVMGetElementType(elt_tys[n]);
}

fn find_scope_cx(cx: @block_ctxt) -> @block_ctxt {
    let cur = cx;
    while true {
        if cur.kind != NON_SCOPE_BLOCK { break; }
        cur = alt check cur.parent { parent_some(b) { b } };
    }
    cur
}

// Accessors
// TODO: When we have overloading, simplify these names!

pure fn bcx_tcx(bcx: @block_ctxt) -> ty::ctxt { ret bcx.fcx.ccx.tcx; }
pure fn bcx_ccx(bcx: @block_ctxt) -> @crate_ctxt { ret bcx.fcx.ccx; }
pure fn bcx_fcx(bcx: @block_ctxt) -> @fn_ctxt { ret bcx.fcx; }
pure fn fcx_ccx(fcx: @fn_ctxt) -> @crate_ctxt { ret fcx.ccx; }
pure fn fcx_tcx(fcx: @fn_ctxt) -> ty::ctxt { ret fcx.ccx.tcx; }
pure fn ccx_tcx(ccx: @crate_ctxt) -> ty::ctxt { ret ccx.tcx; }

// LLVM type constructors.
fn T_void() -> TypeRef {
    // Note: For the time being llvm is kinda busted here, it has the notion
    // of a 'void' type that can only occur as part of the signature of a
    // function, but no general unit type of 0-sized value. This is, afaict,
    // vestigial from its C heritage, and we'll be attempting to submit a
    // patch upstream to fix it. In the mean time we only model function
    // outputs (Rust functions and C functions) using T_void, and model the
    // Rust general purpose nil type you can construct as 1-bit (always
    // zero). This makes the result incorrect for now -- things like a tuple
    // of 10 nil values will have 10-bit size -- but it doesn't seem like we
    // have any other options until it's fixed upstream.

    ret llvm::LLVMVoidType();
}

fn T_nil() -> TypeRef {
    // NB: See above in T_void().

    ret llvm::LLVMInt1Type();
}

fn T_metadata() -> TypeRef { ret llvm::LLVMMetadataType(); }

fn T_i1() -> TypeRef { ret llvm::LLVMInt1Type(); }

fn T_i8() -> TypeRef { ret llvm::LLVMInt8Type(); }

fn T_i16() -> TypeRef { ret llvm::LLVMInt16Type(); }

fn T_i32() -> TypeRef { ret llvm::LLVMInt32Type(); }

fn T_i64() -> TypeRef { ret llvm::LLVMInt64Type(); }

fn T_f32() -> TypeRef { ret llvm::LLVMFloatType(); }

fn T_f64() -> TypeRef { ret llvm::LLVMDoubleType(); }

fn T_bool() -> TypeRef { ret T_i1(); }

fn T_int(targ_cfg: @session::config) -> TypeRef {
    ret alt targ_cfg.arch {
      session::arch_x86 { T_i32() }
      session::arch_x86_64 { T_i64() }
      session::arch_arm { T_i32() }
    };
}

fn T_int_ty(cx: @crate_ctxt, t: ast::int_ty) -> TypeRef {
    alt t {
      ast::ty_i { cx.int_type }
      ast::ty_char { T_char() }
      ast::ty_i8 { T_i8() }
      ast::ty_i16 { T_i16() }
      ast::ty_i32 { T_i32() }
      ast::ty_i64 { T_i64() }
    }
}

fn T_uint_ty(cx: @crate_ctxt, t: ast::uint_ty) -> TypeRef {
    alt t {
      ast::ty_u { cx.int_type }
      ast::ty_u8 { T_i8() }
      ast::ty_u16 { T_i16() }
      ast::ty_u32 { T_i32() }
      ast::ty_u64 { T_i64() }
    }
}

fn T_float_ty(cx: @crate_ctxt, t: ast::float_ty) -> TypeRef {
    alt t {
      ast::ty_f { cx.float_type }
      ast::ty_f32 { T_f32() }
      ast::ty_f64 { T_f64() }
    }
}

fn T_float(targ_cfg: @session::config) -> TypeRef {
    ret alt targ_cfg.arch {
      session::arch_x86 { T_f64() }
      session::arch_x86_64 { T_f64() }
      session::arch_arm { T_f64() }
    };
}

fn T_char() -> TypeRef { ret T_i32(); }

fn T_size_t(targ_cfg: @session::config) -> TypeRef {
    ret T_int(targ_cfg);
}

fn T_fn(inputs: [TypeRef], output: TypeRef) -> TypeRef unsafe {
    ret llvm::LLVMFunctionType(output, to_ptr(inputs),
                               inputs.len() as unsigned,
                               False);
}

fn T_fn_pair(cx: @crate_ctxt, tfn: TypeRef) -> TypeRef {
    ret T_struct([T_ptr(tfn), T_opaque_cbox_ptr(cx)]);
}

fn T_ptr(t: TypeRef) -> TypeRef {
    ret llvm::LLVMPointerType(t, 0u as unsigned);
}

fn T_struct(elts: [TypeRef]) -> TypeRef unsafe {
    ret llvm::LLVMStructType(to_ptr(elts), elts.len() as unsigned, False);
}

fn T_named_struct(name: str) -> TypeRef {
    let c = llvm::LLVMGetGlobalContext();
    ret str::as_buf(name, {|buf| llvm::LLVMStructCreateNamed(c, buf) });
}

fn set_struct_body(t: TypeRef, elts: [TypeRef]) unsafe {
    llvm::LLVMStructSetBody(t, to_ptr(elts),
                            elts.len() as unsigned, False);
}

fn T_empty_struct() -> TypeRef { ret T_struct([]); }

// A dict is, in reality, a vtable pointer followed by zero or more pointers
// to tydescs and other dicts that it closes over. But the types and number of
// those are rarely known to the code that needs to manipulate them, so they
// are described by this opaque type.
fn T_dict() -> TypeRef { T_array(T_ptr(T_i8()), 1u) }

fn T_task(targ_cfg: @session::config) -> TypeRef {
    let t = T_named_struct("task");

    // Refcount
    // Delegate pointer
    // Stack segment pointer
    // Runtime SP
    // Rust SP
    // GC chain


    // Domain pointer
    // Crate cache pointer

    let t_int = T_int(targ_cfg);
    let elems =
        [t_int, t_int, t_int, t_int,
         t_int, t_int, t_int, t_int];
    set_struct_body(t, elems);
    ret t;
}

fn T_tydesc_field(cx: @crate_ctxt, field: int) -> TypeRef unsafe {
    // Bit of a kludge: pick the fn typeref out of the tydesc..

    let tydesc_elts: [TypeRef] =
        vec::init_elt::<TypeRef>(abi::n_tydesc_fields as uint,
                                 T_nil());
    llvm::LLVMGetStructElementTypes(cx.tydesc_type,
                                    to_ptr::<TypeRef>(tydesc_elts));
    let t = llvm::LLVMGetElementType(tydesc_elts[field]);
    ret t;
}

fn T_glue_fn(cx: @crate_ctxt) -> TypeRef {
    let s = "glue_fn";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_tydesc_field(cx, abi::tydesc_field_drop_glue);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_tydesc(targ_cfg: @session::config) -> TypeRef {
    let tydesc = T_named_struct("tydesc");
    let tydescpp = T_ptr(T_ptr(tydesc));
    let pvoid = T_ptr(T_i8());
    let glue_fn_ty =
        T_ptr(T_fn([T_ptr(T_nil()), T_ptr(T_nil()), tydescpp,
                    pvoid], T_void()));

    let int_type = T_int(targ_cfg);
    let elems =
        [tydescpp, int_type, int_type,
         glue_fn_ty, glue_fn_ty, glue_fn_ty,
         T_ptr(T_i8()), glue_fn_ty, glue_fn_ty, glue_fn_ty, T_ptr(T_i8()),
         T_ptr(T_i8()), T_ptr(T_i8()), int_type, int_type];
    set_struct_body(tydesc, elems);
    ret tydesc;
}

fn T_array(t: TypeRef, n: uint) -> TypeRef {
    ret llvm::LLVMArrayType(t, n as unsigned);
}

// Interior vector.
//
// TODO: Support user-defined vector sizes.
fn T_vec2(targ_cfg: @session::config, t: TypeRef) -> TypeRef {
    ret T_struct([T_int(targ_cfg), // fill
                  T_int(targ_cfg), // alloc
                  T_array(t, 0u)]); // elements
}

fn T_vec(ccx: @crate_ctxt, t: TypeRef) -> TypeRef {
    ret T_vec2(ccx.sess.targ_cfg, t);
}

// Note that the size of this one is in bytes.
fn T_opaque_vec(targ_cfg: @session::config) -> TypeRef {
    ret T_vec2(targ_cfg, T_i8());
}

// Let T be the content of a box @T.  tuplify_box_ty(t) returns the
// representation of @T as a tuple (i.e., the ty::t version of what T_box()
// returns).
fn tuplify_box_ty(tcx: ty::ctxt, t: ty::t) -> ty::t {
    ret tuplify_cbox_ty(tcx, t, ty::mk_type(tcx));
}

// As tuplify_box_ty(), but allows the caller to specify what type of type
// descr is embedded in the box (ty::type vs ty::send_type).  This is useful
// for unique closure boxes, hence the name "cbox_ty" (closure box type).
fn tuplify_cbox_ty(tcx: ty::ctxt, t: ty::t, tydesc_t: ty::t) -> ty::t {
    let ptr = ty::mk_ptr(tcx, {ty: ty::mk_nil(tcx), mutbl: ast::m_imm});
    ret ty::mk_tup(tcx, [ty::mk_uint(tcx), tydesc_t,
                         ptr, ptr,
                         t]);
}

fn T_box_header_fields(cx: @crate_ctxt) -> [TypeRef] {
    let ptr = T_ptr(T_i8());
    ret [cx.int_type, T_ptr(cx.tydesc_type), ptr, ptr];
}

fn T_box_header(cx: @crate_ctxt) -> TypeRef {
    ret T_struct(T_box_header_fields(cx));
}

fn T_box(cx: @crate_ctxt, t: TypeRef) -> TypeRef {
    ret T_struct(T_box_header_fields(cx) + [t]);
}

fn T_opaque_box(cx: @crate_ctxt) -> TypeRef {
    ret T_box(cx, T_i8());
}

fn T_opaque_box_ptr(cx: @crate_ctxt) -> TypeRef {
    ret T_ptr(T_opaque_box(cx));
}

fn T_port(cx: @crate_ctxt, _t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type]); // Refcount

}

fn T_chan(cx: @crate_ctxt, _t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type]); // Refcount

}

fn T_taskptr(cx: @crate_ctxt) -> TypeRef { ret T_ptr(cx.task_type); }


// This type must never be used directly; it must always be cast away.
fn T_typaram(tn: type_names) -> TypeRef {
    let s = "typaram";
    alt name_has_type(tn, s) { some(t) { ret t; } _ {} }
    let t = T_i8();
    associate_type(tn, s, t);
    ret t;
}

fn T_typaram_ptr(tn: type_names) -> TypeRef { ret T_ptr(T_typaram(tn)); }

fn T_opaque_cbox_ptr(cx: @crate_ctxt) -> TypeRef {
    // closures look like boxes (even when they are fn~ or fn&)
    // see trans_closure.rs
    ret T_opaque_box_ptr(cx);
}

fn T_enum_variant(cx: @crate_ctxt) -> TypeRef {
    ret cx.int_type;
}

fn T_enum(cx: @crate_ctxt, size: uint) -> TypeRef {
    let s = "enum_" + uint::to_str(size, 10u);
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t =
        if size == 0u {
            T_struct([T_enum_variant(cx)])
        } else { T_struct([T_enum_variant(cx), T_array(T_i8(), size)]) };
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_opaque_enum(cx: @crate_ctxt) -> TypeRef {
    let s = "opaque_enum";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_struct([T_enum_variant(cx), T_i8()]);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_opaque_enum_ptr(cx: @crate_ctxt) -> TypeRef {
    ret T_ptr(T_opaque_enum(cx));
}

fn T_captured_tydescs(cx: @crate_ctxt, n: uint) -> TypeRef {
    ret T_struct(vec::init_elt::<TypeRef>(n, T_ptr(cx.tydesc_type)));
}

fn T_opaque_iface(cx: @crate_ctxt) -> TypeRef {
    T_struct([T_ptr(cx.tydesc_type), T_opaque_box_ptr(cx)])
}

fn T_opaque_port_ptr() -> TypeRef { ret T_ptr(T_i8()); }

fn T_opaque_chan_ptr() -> TypeRef { ret T_ptr(T_i8()); }


// LLVM constant constructors.
fn C_null(t: TypeRef) -> ValueRef { ret llvm::LLVMConstNull(t); }

fn C_integral(t: TypeRef, u: u64, sign_extend: Bool) -> ValueRef {
    let u_hi = (u >> 32u64) as unsigned;
    let u_lo = u as unsigned;
    ret llvm::LLVMRustConstInt(t, u_hi, u_lo, sign_extend);
}

fn C_floating(s: str, t: TypeRef) -> ValueRef {
    ret str::as_buf(s, {|buf| llvm::LLVMConstRealOfString(t, buf) });
}

fn C_nil() -> ValueRef {
    // NB: See comment above in T_void().

    ret C_integral(T_i1(), 0u64, False);
}

fn C_bool(b: bool) -> ValueRef {
    if b {
        ret C_integral(T_bool(), 1u64, False);
    } else { ret C_integral(T_bool(), 0u64, False); }
}

fn C_i32(i: i32) -> ValueRef {
    ret C_integral(T_i32(), i as u64, True);
}

fn C_i64(i: i64) -> ValueRef {
    ret C_integral(T_i64(), i as u64, True);
}

fn C_int(cx: @crate_ctxt, i: int) -> ValueRef {
    ret C_integral(cx.int_type, i as u64, True);
}

fn C_uint(cx: @crate_ctxt, i: uint) -> ValueRef {
    ret C_integral(cx.int_type, i as u64, False);
}

fn C_u8(i: uint) -> ValueRef { ret C_integral(T_i8(), i as u64, False); }


// This is a 'c-like' raw string, which differs from
// our boxed-and-length-annotated strings.
fn C_cstr(cx: @crate_ctxt, s: str) -> ValueRef {
    let sc = str::as_buf(s) {|buf|
        llvm::LLVMConstString(buf, str::len_bytes(s) as unsigned, False)
    };
    let g =
        str::as_buf(cx.names("str"),
                    {|buf| llvm::LLVMAddGlobal(cx.llmod, val_ty(sc), buf) });
    llvm::LLVMSetInitializer(g, sc);
    llvm::LLVMSetGlobalConstant(g, True);
    lib::llvm::SetLinkage(g, lib::llvm::InternalLinkage);
    ret g;
}

// Returns a Plain Old LLVM String:
fn C_postr(s: str) -> ValueRef {
    ret str::as_buf(s) {|buf|
        llvm::LLVMConstString(buf, str::len_bytes(s) as unsigned, False)
    };
}

fn C_zero_byte_arr(size: uint) -> ValueRef unsafe {
    let i = 0u;
    let elts: [ValueRef] = [];
    while i < size { elts += [C_u8(0u)]; i += 1u; }
    ret llvm::LLVMConstArray(T_i8(), vec::to_ptr(elts),
                             elts.len() as unsigned);
}

fn C_struct(elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstStruct(vec::to_ptr(elts), elts.len() as unsigned,
                              False);
}

fn C_named_struct(T: TypeRef, elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstNamedStruct(T, vec::to_ptr(elts),
                                   elts.len() as unsigned);
}

fn C_array(ty: TypeRef, elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstArray(ty, vec::to_ptr(elts),
                             elts.len() as unsigned);
}

fn C_bytes(bytes: [u8]) -> ValueRef unsafe {
    ret llvm::LLVMConstString(
        unsafe::reinterpret_cast(vec::to_ptr(bytes)),
        bytes.len() as unsigned, False);
}

fn C_shape(ccx: @crate_ctxt, bytes: [u8]) -> ValueRef {
    let llshape = C_bytes(bytes);
    let llglobal = str::as_buf(ccx.names("shape"), {|buf|
        llvm::LLVMAddGlobal(ccx.llmod, val_ty(llshape), buf)
    });
    llvm::LLVMSetInitializer(llglobal, llshape);
    llvm::LLVMSetGlobalConstant(llglobal, True);
    lib::llvm::SetLinkage(llglobal, lib::llvm::InternalLinkage);
    ret llvm::LLVMConstPointerCast(llglobal, T_ptr(T_i8()));
}


pure fn valid_variant_index(ix: uint, cx: @block_ctxt, enum_id: ast::def_id,
                            variant_id: ast::def_id) -> bool {

    // Handwaving: it's ok to pretend this code is referentially
    // transparent, because the relevant parts of the type context don't
    // change. (We're not adding new variants during trans.)
    unchecked{
        let variant =
            ty::enum_variant_with_id(bcx_tcx(cx), enum_id, variant_id);
        ix < variant.args.len()
    }
}

pure fn type_has_static_size(cx: @crate_ctxt, t: ty::t) -> bool {
    !ty::type_has_dynamic_size(cx.tcx, t)
}

// Used to identify cached dictionaries
enum dict_param {
    dict_param_dict(dict_id),
    dict_param_ty(ty::t),
}
type dict_id = @{def: ast::def_id, params: [dict_param]};
fn hash_dict_id(&&dp: dict_id) -> uint {
    let h = syntax::ast_util::hash_def_id(dp.def);
    for param in dp.params {
        h = h << 2u;
        alt param {
          dict_param_dict(d) { h += hash_dict_id(d); }
          dict_param_ty(t) { h += ty::type_id(t); }
        }
    }
    h
}

// Used to identify cached monomorphized functions
type mono_id = @{def: ast::def_id, substs: [ty::t], dicts: [dict_id]};
fn hash_mono_id(&&mi: mono_id) -> uint {
    let h = syntax::ast_util::hash_def_id(mi.def);
    for ty in mi.substs { h = (h << 2u) + ty::type_id(ty); }
    for dict in mi.dicts { h = (h << 2u) + hash_dict_id(dict); }
    h
}

fn umax(cx: @block_ctxt, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = build::ICmp(cx, lib::llvm::IntULT, a, b);
    ret build::Select(cx, cond, b, a);
}

fn umin(cx: @block_ctxt, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = build::ICmp(cx, lib::llvm::IntULT, a, b);
    ret build::Select(cx, cond, a, b);
}

fn align_to(cx: @block_ctxt, off: ValueRef, align: ValueRef) -> ValueRef {
    let mask = build::Sub(cx, align, C_int(bcx_ccx(cx), 1));
    let bumped = build::Add(cx, off, mask);
    ret build::And(cx, bumped, build::Not(cx, mask));
}

fn path_str(p: path) -> str {
    let r = "", first = true;
    for e in p {
        alt e { ast_map::path_name(s) | ast_map::path_mod(s) {
          if first { first = false; }
          else { r += "::"; }
          r += s;
        } }
    }
    r
}

fn node_id_type(bcx: @block_ctxt, id: ast::node_id) -> ty::t {
    let tcx = bcx_tcx(bcx);
    let t = ty::node_id_to_type(tcx, id);
    alt bcx.fcx.param_substs {
      some(substs) { ty::substitute_type_params(tcx, substs.tys, t) }
      _ { t }
    }
}
fn expr_ty(bcx: @block_ctxt, ex: @ast::expr) -> ty::t {
    node_id_type(bcx, ex.id)
}
fn node_id_type_params(bcx: @block_ctxt, id: ast::node_id) -> [ty::t] {
    let tcx = bcx_tcx(bcx);
    let params = ty::node_id_to_type_params(tcx, id);
    alt bcx.fcx.param_substs {
      some(substs) {
        vec::map(params) {|t| ty::substitute_type_params(tcx, substs.tys, t) }
      }
      _ { params }
    }
}

//
// Local Variables:
// mode: rust
// fill-column: 78;
// indent-tabs-mode: nil
// c-basic-offset: 4
// buffer-file-coding-system: utf-8-unix
// End:
//
