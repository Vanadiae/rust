//! MIR datatypes and passes. See the [rustc dev guide] for more info.
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/mir/index.html

use crate::mir::interpret::{AllocRange, ConstAllocation, ErrorHandled, Scalar};
use crate::mir::visit::MirVisitable;
use crate::ty::codec::{TyDecoder, TyEncoder};
use crate::ty::fold::{FallibleTypeFolder, TypeFoldable};
use crate::ty::print::{pretty_print_const, with_no_trimmed_paths};
use crate::ty::print::{FmtPrinter, Printer};
use crate::ty::visit::TypeVisitableExt;
use crate::ty::{self, List, Ty, TyCtxt};
use crate::ty::{AdtDef, InstanceDef, UserTypeAnnotationIndex};
use crate::ty::{GenericArg, GenericArgsRef};

use rustc_data_structures::captures::Captures;
use rustc_errors::{DiagnosticArgValue, DiagnosticMessage, ErrorGuaranteed, IntoDiagnosticArg};
use rustc_hir::def::{CtorKind, Namespace};
use rustc_hir::def_id::{DefId, CRATE_DEF_ID};
use rustc_hir::{self, GeneratorKind, ImplicitSelfKind};
use rustc_hir::{self as hir, HirId};
use rustc_session::Session;
use rustc_target::abi::{FieldIdx, VariantIdx};

use polonius_engine::Atom;
pub use rustc_ast::Mutability;
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::fx::FxHashSet;
use rustc_data_structures::graph::dominators::Dominators;
use rustc_index::{Idx, IndexSlice, IndexVec};
use rustc_serialize::{Decodable, Encodable};
use rustc_span::symbol::Symbol;
use rustc_span::{Span, DUMMY_SP};

use either::Either;

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::fmt::{self, Debug, Formatter};
use std::ops::{Index, IndexMut};
use std::{iter, mem};

pub use self::query::*;
pub use basic_blocks::BasicBlocks;

mod basic_blocks;
mod consts;
pub mod coverage;
mod generic_graph;
pub mod generic_graphviz;
pub mod graphviz;
pub mod interpret;
pub mod mono;
pub mod patch;
pub mod pretty;
mod query;
pub mod spanview;
mod statement;
mod syntax;
pub mod tcx;
mod terminator;

pub mod traversal;
mod type_foldable;
pub mod visit;

pub use self::generic_graph::graphviz_safe_def_name;
pub use self::graphviz::write_mir_graphviz;
pub use self::pretty::{
    create_dump_file, display_allocation, dump_enabled, dump_mir, write_mir_pretty, PassWhere,
};
pub use consts::*;
use pretty::pretty_print_const_value;
pub use statement::*;
pub use syntax::*;
pub use terminator::*;

/// Types for locals
pub type LocalDecls<'tcx> = IndexSlice<Local, LocalDecl<'tcx>>;

pub trait HasLocalDecls<'tcx> {
    fn local_decls(&self) -> &LocalDecls<'tcx>;
}

impl<'tcx> HasLocalDecls<'tcx> for IndexVec<Local, LocalDecl<'tcx>> {
    #[inline]
    fn local_decls(&self) -> &LocalDecls<'tcx> {
        self
    }
}

impl<'tcx> HasLocalDecls<'tcx> for LocalDecls<'tcx> {
    #[inline]
    fn local_decls(&self) -> &LocalDecls<'tcx> {
        self
    }
}

impl<'tcx> HasLocalDecls<'tcx> for Body<'tcx> {
    #[inline]
    fn local_decls(&self) -> &LocalDecls<'tcx> {
        &self.local_decls
    }
}

thread_local! {
    static PASS_NAMES: RefCell<FxHashMap<&'static str, &'static str>> = {
        RefCell::new(FxHashMap::default())
    };
}

/// Converts a MIR pass name into a snake case form to match the profiling naming style.
fn to_profiler_name(type_name: &'static str) -> &'static str {
    PASS_NAMES.with(|names| match names.borrow_mut().entry(type_name) {
        Entry::Occupied(e) => *e.get(),
        Entry::Vacant(e) => {
            let snake_case: String = type_name
                .chars()
                .flat_map(|c| {
                    if c.is_ascii_uppercase() {
                        vec!['_', c.to_ascii_lowercase()]
                    } else if c == '-' {
                        vec!['_']
                    } else {
                        vec![c]
                    }
                })
                .collect();
            let result = &*String::leak(format!("mir_pass{}", snake_case));
            e.insert(result);
            result
        }
    })
}

/// A streamlined trait that you can implement to create a pass; the
/// pass will be named after the type, and it will consist of a main
/// loop that goes over each available MIR and applies `run_pass`.
pub trait MirPass<'tcx> {
    fn name(&self) -> &'static str {
        let name = std::any::type_name::<Self>();
        if let Some((_, tail)) = name.rsplit_once(':') { tail } else { name }
    }

    fn profiler_name(&self) -> &'static str {
        to_profiler_name(self.name())
    }

    /// Returns `true` if this pass is enabled with the current combination of compiler flags.
    fn is_enabled(&self, _sess: &Session) -> bool {
        true
    }

    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>);

    fn is_mir_dump_enabled(&self) -> bool {
        true
    }
}

impl MirPhase {
    /// Gets the index of the current MirPhase within the set of all `MirPhase`s.
    ///
    /// FIXME(JakobDegen): Return a `(usize, usize)` instead.
    pub fn phase_index(&self) -> usize {
        const BUILT_PHASE_COUNT: usize = 1;
        const ANALYSIS_PHASE_COUNT: usize = 2;
        match self {
            MirPhase::Built => 1,
            MirPhase::Analysis(analysis_phase) => {
                1 + BUILT_PHASE_COUNT + (*analysis_phase as usize)
            }
            MirPhase::Runtime(runtime_phase) => {
                1 + BUILT_PHASE_COUNT + ANALYSIS_PHASE_COUNT + (*runtime_phase as usize)
            }
        }
    }

    /// Parses an `MirPhase` from a pair of strings. Panics if this isn't possible for any reason.
    pub fn parse(dialect: String, phase: Option<String>) -> Self {
        match &*dialect.to_ascii_lowercase() {
            "built" => {
                assert!(phase.is_none(), "Cannot specify a phase for `Built` MIR");
                MirPhase::Built
            }
            "analysis" => Self::Analysis(AnalysisPhase::parse(phase)),
            "runtime" => Self::Runtime(RuntimePhase::parse(phase)),
            _ => bug!("Unknown MIR dialect: '{}'", dialect),
        }
    }
}

impl AnalysisPhase {
    pub fn parse(phase: Option<String>) -> Self {
        let Some(phase) = phase else {
            return Self::Initial;
        };

        match &*phase.to_ascii_lowercase() {
            "initial" => Self::Initial,
            "post_cleanup" | "post-cleanup" | "postcleanup" => Self::PostCleanup,
            _ => bug!("Unknown analysis phase: '{}'", phase),
        }
    }
}

impl RuntimePhase {
    pub fn parse(phase: Option<String>) -> Self {
        let Some(phase) = phase else {
            return Self::Initial;
        };

        match &*phase.to_ascii_lowercase() {
            "initial" => Self::Initial,
            "post_cleanup" | "post-cleanup" | "postcleanup" => Self::PostCleanup,
            "optimized" => Self::Optimized,
            _ => bug!("Unknown runtime phase: '{}'", phase),
        }
    }
}

/// Where a specific `mir::Body` comes from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[derive(HashStable, TyEncodable, TyDecodable, TypeFoldable, TypeVisitable)]
pub struct MirSource<'tcx> {
    pub instance: InstanceDef<'tcx>,

    /// If `Some`, this is a promoted rvalue within the parent function.
    pub promoted: Option<Promoted>,
}

impl<'tcx> MirSource<'tcx> {
    pub fn item(def_id: DefId) -> Self {
        MirSource { instance: InstanceDef::Item(def_id), promoted: None }
    }

    pub fn from_instance(instance: InstanceDef<'tcx>) -> Self {
        MirSource { instance, promoted: None }
    }

    #[inline]
    pub fn def_id(&self) -> DefId {
        self.instance.def_id()
    }
}

#[derive(Clone, TyEncodable, TyDecodable, Debug, HashStable, TypeFoldable, TypeVisitable)]
pub struct GeneratorInfo<'tcx> {
    /// The yield type of the function, if it is a generator.
    pub yield_ty: Option<Ty<'tcx>>,

    /// Generator drop glue.
    pub generator_drop: Option<Body<'tcx>>,

    /// The layout of a generator. Produced by the state transformation.
    pub generator_layout: Option<GeneratorLayout<'tcx>>,

    /// If this is a generator then record the type of source expression that caused this generator
    /// to be created.
    pub generator_kind: GeneratorKind,
}

/// The lowered representation of a single function.
#[derive(Clone, TyEncodable, TyDecodable, Debug, HashStable, TypeFoldable, TypeVisitable)]
pub struct Body<'tcx> {
    /// A list of basic blocks. References to basic block use a newtyped index type [`BasicBlock`]
    /// that indexes into this vector.
    pub basic_blocks: BasicBlocks<'tcx>,

    /// Records how far through the "desugaring and optimization" process this particular
    /// MIR has traversed. This is particularly useful when inlining, since in that context
    /// we instantiate the promoted constants and add them to our promoted vector -- but those
    /// promoted items have already been optimized, whereas ours have not. This field allows
    /// us to see the difference and forego optimization on the inlined promoted items.
    pub phase: MirPhase,

    /// How many passses we have executed since starting the current phase. Used for debug output.
    pub pass_count: usize,

    pub source: MirSource<'tcx>,

    /// A list of source scopes; these are referenced by statements
    /// and used for debuginfo. Indexed by a `SourceScope`.
    pub source_scopes: IndexVec<SourceScope, SourceScopeData<'tcx>>,

    pub generator: Option<Box<GeneratorInfo<'tcx>>>,

    /// Declarations of locals.
    ///
    /// The first local is the return value pointer, followed by `arg_count`
    /// locals for the function arguments, followed by any user-declared
    /// variables and temporaries.
    pub local_decls: IndexVec<Local, LocalDecl<'tcx>>,

    /// User type annotations.
    pub user_type_annotations: ty::CanonicalUserTypeAnnotations<'tcx>,

    /// The number of arguments this function takes.
    ///
    /// Starting at local 1, `arg_count` locals will be provided by the caller
    /// and can be assumed to be initialized.
    ///
    /// If this MIR was built for a constant, this will be 0.
    pub arg_count: usize,

    /// Mark an argument local (which must be a tuple) as getting passed as
    /// its individual components at the LLVM level.
    ///
    /// This is used for the "rust-call" ABI.
    pub spread_arg: Option<Local>,

    /// Debug information pertaining to user variables, including captures.
    pub var_debug_info: Vec<VarDebugInfo<'tcx>>,

    /// A span representing this MIR, for error reporting.
    pub span: Span,

    /// Constants that are required to evaluate successfully for this MIR to be well-formed.
    /// We hold in this field all the constants we are not able to evaluate yet.
    pub required_consts: Vec<Constant<'tcx>>,

    /// Does this body use generic parameters. This is used for the `ConstEvaluatable` check.
    ///
    /// Note that this does not actually mean that this body is not computable right now.
    /// The repeat count in the following example is polymorphic, but can still be evaluated
    /// without knowing anything about the type parameter `T`.
    ///
    /// ```rust
    /// fn test<T>() {
    ///     let _ = [0; std::mem::size_of::<*mut T>()];
    /// }
    /// ```
    ///
    /// **WARNING**: Do not change this flags after the MIR was originally created, even if an optimization
    /// removed the last mention of all generic params. We do not want to rely on optimizations and
    /// potentially allow things like `[u8; std::mem::size_of::<T>() * 0]` due to this.
    pub is_polymorphic: bool,

    /// The phase at which this MIR should be "injected" into the compilation process.
    ///
    /// Everything that comes before this `MirPhase` should be skipped.
    ///
    /// This is only `Some` if the function that this body comes from was annotated with `rustc_custom_mir`.
    pub injection_phase: Option<MirPhase>,

    pub tainted_by_errors: Option<ErrorGuaranteed>,
}

impl<'tcx> Body<'tcx> {
    pub fn new(
        source: MirSource<'tcx>,
        basic_blocks: IndexVec<BasicBlock, BasicBlockData<'tcx>>,
        source_scopes: IndexVec<SourceScope, SourceScopeData<'tcx>>,
        local_decls: IndexVec<Local, LocalDecl<'tcx>>,
        user_type_annotations: ty::CanonicalUserTypeAnnotations<'tcx>,
        arg_count: usize,
        var_debug_info: Vec<VarDebugInfo<'tcx>>,
        span: Span,
        generator_kind: Option<GeneratorKind>,
        tainted_by_errors: Option<ErrorGuaranteed>,
    ) -> Self {
        // We need `arg_count` locals, and one for the return place.
        assert!(
            local_decls.len() > arg_count,
            "expected at least {} locals, got {}",
            arg_count + 1,
            local_decls.len()
        );

        let mut body = Body {
            phase: MirPhase::Built,
            pass_count: 0,
            source,
            basic_blocks: BasicBlocks::new(basic_blocks),
            source_scopes,
            generator: generator_kind.map(|generator_kind| {
                Box::new(GeneratorInfo {
                    yield_ty: None,
                    generator_drop: None,
                    generator_layout: None,
                    generator_kind,
                })
            }),
            local_decls,
            user_type_annotations,
            arg_count,
            spread_arg: None,
            var_debug_info,
            span,
            required_consts: Vec::new(),
            is_polymorphic: false,
            injection_phase: None,
            tainted_by_errors,
        };
        body.is_polymorphic = body.has_non_region_param();
        body
    }

    /// Returns a partially initialized MIR body containing only a list of basic blocks.
    ///
    /// The returned MIR contains no `LocalDecl`s (even for the return place) or source scopes. It
    /// is only useful for testing but cannot be `#[cfg(test)]` because it is used in a different
    /// crate.
    pub fn new_cfg_only(basic_blocks: IndexVec<BasicBlock, BasicBlockData<'tcx>>) -> Self {
        let mut body = Body {
            phase: MirPhase::Built,
            pass_count: 0,
            source: MirSource::item(CRATE_DEF_ID.to_def_id()),
            basic_blocks: BasicBlocks::new(basic_blocks),
            source_scopes: IndexVec::new(),
            generator: None,
            local_decls: IndexVec::new(),
            user_type_annotations: IndexVec::new(),
            arg_count: 0,
            spread_arg: None,
            span: DUMMY_SP,
            required_consts: Vec::new(),
            var_debug_info: Vec::new(),
            is_polymorphic: false,
            injection_phase: None,
            tainted_by_errors: None,
        };
        body.is_polymorphic = body.has_non_region_param();
        body
    }

    #[inline]
    pub fn basic_blocks_mut(&mut self) -> &mut IndexVec<BasicBlock, BasicBlockData<'tcx>> {
        self.basic_blocks.as_mut()
    }

    #[inline]
    pub fn local_kind(&self, local: Local) -> LocalKind {
        let index = local.as_usize();
        if index == 0 {
            debug_assert!(
                self.local_decls[local].mutability == Mutability::Mut,
                "return place should be mutable"
            );

            LocalKind::ReturnPointer
        } else if index < self.arg_count + 1 {
            LocalKind::Arg
        } else {
            LocalKind::Temp
        }
    }

    /// Returns an iterator over all user-declared mutable locals.
    #[inline]
    pub fn mut_vars_iter<'a>(&'a self) -> impl Iterator<Item = Local> + Captures<'tcx> + 'a {
        (self.arg_count + 1..self.local_decls.len()).filter_map(move |index| {
            let local = Local::new(index);
            let decl = &self.local_decls[local];
            (decl.is_user_variable() && decl.mutability.is_mut()).then_some(local)
        })
    }

    /// Returns an iterator over all user-declared mutable arguments and locals.
    #[inline]
    pub fn mut_vars_and_args_iter<'a>(
        &'a self,
    ) -> impl Iterator<Item = Local> + Captures<'tcx> + 'a {
        (1..self.local_decls.len()).filter_map(move |index| {
            let local = Local::new(index);
            let decl = &self.local_decls[local];
            if (decl.is_user_variable() || index < self.arg_count + 1)
                && decl.mutability == Mutability::Mut
            {
                Some(local)
            } else {
                None
            }
        })
    }

    /// Returns an iterator over all function arguments.
    #[inline]
    pub fn args_iter(&self) -> impl Iterator<Item = Local> + ExactSizeIterator {
        (1..self.arg_count + 1).map(Local::new)
    }

    /// Returns an iterator over all user-defined variables and compiler-generated temporaries (all
    /// locals that are neither arguments nor the return place).
    #[inline]
    pub fn vars_and_temps_iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = Local> + ExactSizeIterator {
        (self.arg_count + 1..self.local_decls.len()).map(Local::new)
    }

    #[inline]
    pub fn drain_vars_and_temps<'a>(&'a mut self) -> impl Iterator<Item = LocalDecl<'tcx>> + 'a {
        self.local_decls.drain(self.arg_count + 1..)
    }

    /// Returns the source info associated with `location`.
    pub fn source_info(&self, location: Location) -> &SourceInfo {
        let block = &self[location.block];
        let stmts = &block.statements;
        let idx = location.statement_index;
        if idx < stmts.len() {
            &stmts[idx].source_info
        } else {
            assert_eq!(idx, stmts.len());
            &block.terminator().source_info
        }
    }

    /// Returns the return type; it always return first element from `local_decls` array.
    #[inline]
    pub fn return_ty(&self) -> Ty<'tcx> {
        self.local_decls[RETURN_PLACE].ty
    }

    /// Returns the return type; it always return first element from `local_decls` array.
    #[inline]
    pub fn bound_return_ty(&self) -> ty::EarlyBinder<Ty<'tcx>> {
        ty::EarlyBinder::bind(self.local_decls[RETURN_PLACE].ty)
    }

    /// Gets the location of the terminator for the given block.
    #[inline]
    pub fn terminator_loc(&self, bb: BasicBlock) -> Location {
        Location { block: bb, statement_index: self[bb].statements.len() }
    }

    pub fn stmt_at(&self, location: Location) -> Either<&Statement<'tcx>, &Terminator<'tcx>> {
        let Location { block, statement_index } = location;
        let block_data = &self.basic_blocks[block];
        block_data
            .statements
            .get(statement_index)
            .map(Either::Left)
            .unwrap_or_else(|| Either::Right(block_data.terminator()))
    }

    #[inline]
    pub fn yield_ty(&self) -> Option<Ty<'tcx>> {
        self.generator.as_ref().and_then(|generator| generator.yield_ty)
    }

    #[inline]
    pub fn generator_layout(&self) -> Option<&GeneratorLayout<'tcx>> {
        self.generator.as_ref().and_then(|generator| generator.generator_layout.as_ref())
    }

    #[inline]
    pub fn generator_drop(&self) -> Option<&Body<'tcx>> {
        self.generator.as_ref().and_then(|generator| generator.generator_drop.as_ref())
    }

    #[inline]
    pub fn generator_kind(&self) -> Option<GeneratorKind> {
        self.generator.as_ref().map(|generator| generator.generator_kind)
    }

    #[inline]
    pub fn should_skip(&self) -> bool {
        let Some(injection_phase) = self.injection_phase else {
            return false;
        };
        injection_phase > self.phase
    }

    #[inline]
    pub fn is_custom_mir(&self) -> bool {
        self.injection_phase.is_some()
    }

    /// *Must* be called once the full substitution for this body is known, to ensure that the body
    /// is indeed fit for code generation or consumption more generally.
    ///
    /// Sadly there's no nice way to represent an "arbitrary normalizer", so we take one for
    /// constants specifically. (`Option<GenericArgsRef>` could be used for that, but the fact
    /// that `Instance::args_for_mir_body` is private and instead instance exposes normalization
    /// functions makes it seem like exposing the generic args is not the intended strategy.)
    ///
    /// Also sadly, CTFE doesn't even know whether it runs on MIR that is already polymorphic or still monomorphic,
    /// so we cannot just immediately ICE on TooGeneric.
    ///
    /// Returns Ok(()) if everything went fine, and `Err` if a problem occurred and got reported.
    pub fn post_mono_checks(
        &self,
        tcx: TyCtxt<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        normalize_const: impl Fn(ConstantKind<'tcx>) -> Result<ConstantKind<'tcx>, ErrorHandled>,
    ) -> Result<(), ErrorHandled> {
        // For now, the only thing we have to check is is to ensure that all the constants used in
        // the body successfully evaluate.
        for &const_ in &self.required_consts {
            let c = normalize_const(const_.literal)?;
            c.eval(tcx, param_env, Some(const_.span))?;
        }

        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, TyEncodable, TyDecodable, HashStable)]
pub enum Safety {
    Safe,
    /// Unsafe because of compiler-generated unsafe code, like `await` desugaring
    BuiltinUnsafe,
    /// Unsafe because of an unsafe fn
    FnUnsafe,
    /// Unsafe because of an `unsafe` block
    ExplicitUnsafe(hir::HirId),
}

impl<'tcx> Index<BasicBlock> for Body<'tcx> {
    type Output = BasicBlockData<'tcx>;

    #[inline]
    fn index(&self, index: BasicBlock) -> &BasicBlockData<'tcx> {
        &self.basic_blocks[index]
    }
}

impl<'tcx> IndexMut<BasicBlock> for Body<'tcx> {
    #[inline]
    fn index_mut(&mut self, index: BasicBlock) -> &mut BasicBlockData<'tcx> {
        &mut self.basic_blocks.as_mut()[index]
    }
}

#[derive(Copy, Clone, Debug, HashStable, TypeFoldable, TypeVisitable)]
pub enum ClearCrossCrate<T> {
    Clear,
    Set(T),
}

impl<T> ClearCrossCrate<T> {
    pub fn as_ref(&self) -> ClearCrossCrate<&T> {
        match self {
            ClearCrossCrate::Clear => ClearCrossCrate::Clear,
            ClearCrossCrate::Set(v) => ClearCrossCrate::Set(v),
        }
    }

    pub fn as_mut(&mut self) -> ClearCrossCrate<&mut T> {
        match self {
            ClearCrossCrate::Clear => ClearCrossCrate::Clear,
            ClearCrossCrate::Set(v) => ClearCrossCrate::Set(v),
        }
    }

    pub fn assert_crate_local(self) -> T {
        match self {
            ClearCrossCrate::Clear => bug!("unwrapping cross-crate data"),
            ClearCrossCrate::Set(v) => v,
        }
    }
}

const TAG_CLEAR_CROSS_CRATE_CLEAR: u8 = 0;
const TAG_CLEAR_CROSS_CRATE_SET: u8 = 1;

impl<E: TyEncoder, T: Encodable<E>> Encodable<E> for ClearCrossCrate<T> {
    #[inline]
    fn encode(&self, e: &mut E) {
        if E::CLEAR_CROSS_CRATE {
            return;
        }

        match *self {
            ClearCrossCrate::Clear => TAG_CLEAR_CROSS_CRATE_CLEAR.encode(e),
            ClearCrossCrate::Set(ref val) => {
                TAG_CLEAR_CROSS_CRATE_SET.encode(e);
                val.encode(e);
            }
        }
    }
}
impl<D: TyDecoder, T: Decodable<D>> Decodable<D> for ClearCrossCrate<T> {
    #[inline]
    fn decode(d: &mut D) -> ClearCrossCrate<T> {
        if D::CLEAR_CROSS_CRATE {
            return ClearCrossCrate::Clear;
        }

        let discr = u8::decode(d);

        match discr {
            TAG_CLEAR_CROSS_CRATE_CLEAR => ClearCrossCrate::Clear,
            TAG_CLEAR_CROSS_CRATE_SET => {
                let val = T::decode(d);
                ClearCrossCrate::Set(val)
            }
            tag => panic!("Invalid tag for ClearCrossCrate: {tag:?}"),
        }
    }
}

/// Grouped information about the source code origin of a MIR entity.
/// Intended to be inspected by diagnostics and debuginfo.
/// Most passes can work with it as a whole, within a single function.
// The unofficial Cranelift backend, at least as of #65828, needs `SourceInfo` to implement `Eq` and
// `Hash`. Please ping @bjorn3 if removing them.
#[derive(Copy, Clone, Debug, Eq, PartialEq, TyEncodable, TyDecodable, Hash, HashStable)]
pub struct SourceInfo {
    /// The source span for the AST pertaining to this MIR entity.
    pub span: Span,

    /// The source scope, keeping track of which bindings can be
    /// seen by debuginfo, active lint levels, `unsafe {...}`, etc.
    pub scope: SourceScope,
}

impl SourceInfo {
    #[inline]
    pub fn outermost(span: Span) -> Self {
        SourceInfo { span, scope: OUTERMOST_SOURCE_SCOPE }
    }
}

///////////////////////////////////////////////////////////////////////////
// Variables and temps

rustc_index::newtype_index! {
    #[derive(HashStable)]
    #[debug_format = "_{}"]
    pub struct Local {
        const RETURN_PLACE = 0;
    }
}

impl Atom for Local {
    fn index(self) -> usize {
        Idx::index(self)
    }
}

/// Classifies locals into categories. See `Body::local_kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, HashStable)]
pub enum LocalKind {
    /// User-declared variable binding or compiler-introduced temporary.
    Temp,
    /// Function argument.
    Arg,
    /// Location of function's return value.
    ReturnPointer,
}

#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable)]
pub struct VarBindingForm<'tcx> {
    /// Is variable bound via `x`, `mut x`, `ref x`, or `ref mut x`?
    pub binding_mode: ty::BindingMode,
    /// If an explicit type was provided for this variable binding,
    /// this holds the source Span of that type.
    ///
    /// NOTE: if you want to change this to a `HirId`, be wary that
    /// doing so breaks incremental compilation (as of this writing),
    /// while a `Span` does not cause our tests to fail.
    pub opt_ty_info: Option<Span>,
    /// Place of the RHS of the =, or the subject of the `match` where this
    /// variable is initialized. None in the case of `let PATTERN;`.
    /// Some((None, ..)) in the case of and `let [mut] x = ...` because
    /// (a) the right-hand side isn't evaluated as a place expression.
    /// (b) it gives a way to separate this case from the remaining cases
    ///     for diagnostics.
    pub opt_match_place: Option<(Option<Place<'tcx>>, Span)>,
    /// The span of the pattern in which this variable was bound.
    pub pat_span: Span,
}

#[derive(Clone, Debug, TyEncodable, TyDecodable)]
pub enum BindingForm<'tcx> {
    /// This is a binding for a non-`self` binding, or a `self` that has an explicit type.
    Var(VarBindingForm<'tcx>),
    /// Binding for a `self`/`&self`/`&mut self` binding where the type is implicit.
    ImplicitSelf(ImplicitSelfKind),
    /// Reference used in a guard expression to ensure immutability.
    RefForGuard,
}

TrivialTypeTraversalImpls! { BindingForm<'tcx> }

mod binding_form_impl {
    use rustc_data_structures::stable_hasher::{HashStable, StableHasher};
    use rustc_query_system::ich::StableHashingContext;

    impl<'a, 'tcx> HashStable<StableHashingContext<'a>> for super::BindingForm<'tcx> {
        fn hash_stable(&self, hcx: &mut StableHashingContext<'a>, hasher: &mut StableHasher) {
            use super::BindingForm::*;
            std::mem::discriminant(self).hash_stable(hcx, hasher);

            match self {
                Var(binding) => binding.hash_stable(hcx, hasher),
                ImplicitSelf(kind) => kind.hash_stable(hcx, hasher),
                RefForGuard => (),
            }
        }
    }
}

/// `BlockTailInfo` is attached to the `LocalDecl` for temporaries
/// created during evaluation of expressions in a block tail
/// expression; that is, a block like `{ STMT_1; STMT_2; EXPR }`.
///
/// It is used to improve diagnostics when such temporaries are
/// involved in borrow_check errors, e.g., explanations of where the
/// temporaries come from, when their destructors are run, and/or how
/// one might revise the code to satisfy the borrow checker's rules.
#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable)]
pub struct BlockTailInfo {
    /// If `true`, then the value resulting from evaluating this tail
    /// expression is ignored by the block's expression context.
    ///
    /// Examples include `{ ...; tail };` and `let _ = { ...; tail };`
    /// but not e.g., `let _x = { ...; tail };`
    pub tail_result_is_ignored: bool,

    /// `Span` of the tail expression.
    pub span: Span,
}

/// A MIR local.
///
/// This can be a binding declared by the user, a temporary inserted by the compiler, a function
/// argument, or the return place.
#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub struct LocalDecl<'tcx> {
    /// Whether this is a mutable binding (i.e., `let x` or `let mut x`).
    ///
    /// Temporaries and the return place are always mutable.
    pub mutability: Mutability,

    // FIXME(matthewjasper) Don't store in this in `Body`
    pub local_info: ClearCrossCrate<Box<LocalInfo<'tcx>>>,

    /// `true` if this is an internal local.
    ///
    /// These locals are not based on types in the source code and are only used
    /// for a few desugarings at the moment.
    ///
    /// The generator transformation will sanity check the locals which are live
    /// across a suspension point against the type components of the generator
    /// which type checking knows are live across a suspension point. We need to
    /// flag drop flags to avoid triggering this check as they are introduced
    /// outside of type inference.
    ///
    /// This should be sound because the drop flags are fully algebraic, and
    /// therefore don't affect the auto-trait or outlives properties of the
    /// generator.
    pub internal: bool,

    /// The type of this local.
    pub ty: Ty<'tcx>,

    /// If the user manually ascribed a type to this variable,
    /// e.g., via `let x: T`, then we carry that type here. The MIR
    /// borrow checker needs this information since it can affect
    /// region inference.
    // FIXME(matthewjasper) Don't store in this in `Body`
    pub user_ty: Option<Box<UserTypeProjections>>,

    /// The *syntactic* (i.e., not visibility) source scope the local is defined
    /// in. If the local was defined in a let-statement, this
    /// is *within* the let-statement, rather than outside
    /// of it.
    ///
    /// This is needed because the visibility source scope of locals within
    /// a let-statement is weird.
    ///
    /// The reason is that we want the local to be *within* the let-statement
    /// for lint purposes, but we want the local to be *after* the let-statement
    /// for names-in-scope purposes.
    ///
    /// That's it, if we have a let-statement like the one in this
    /// function:
    ///
    /// ```
    /// fn foo(x: &str) {
    ///     #[allow(unused_mut)]
    ///     let mut x: u32 = { // <- one unused mut
    ///         let mut y: u32 = x.parse().unwrap();
    ///         y + 2
    ///     };
    ///     drop(x);
    /// }
    /// ```
    ///
    /// Then, from a lint point of view, the declaration of `x: u32`
    /// (and `y: u32`) are within the `#[allow(unused_mut)]` scope - the
    /// lint scopes are the same as the AST/HIR nesting.
    ///
    /// However, from a name lookup point of view, the scopes look more like
    /// as if the let-statements were `match` expressions:
    ///
    /// ```
    /// fn foo(x: &str) {
    ///     match {
    ///         match x.parse::<u32>().unwrap() {
    ///             y => y + 2
    ///         }
    ///     } {
    ///         x => drop(x)
    ///     };
    /// }
    /// ```
    ///
    /// We care about the name-lookup scopes for debuginfo - if the
    /// debuginfo instruction pointer is at the call to `x.parse()`, we
    /// want `x` to refer to `x: &str`, but if it is at the call to
    /// `drop(x)`, we want it to refer to `x: u32`.
    ///
    /// To allow both uses to work, we need to have more than a single scope
    /// for a local. We have the `source_info.scope` represent the "syntactic"
    /// lint scope (with a variable being under its let block) while the
    /// `var_debug_info.source_info.scope` represents the "local variable"
    /// scope (where the "rest" of a block is under all prior let-statements).
    ///
    /// The end result looks like this:
    ///
    /// ```text
    /// ROOT SCOPE
    ///  │{ argument x: &str }
    ///  │
    ///  │ │{ #[allow(unused_mut)] } // This is actually split into 2 scopes
    ///  │ │                         // in practice because I'm lazy.
    ///  │ │
    ///  │ │← x.source_info.scope
    ///  │ │← `x.parse().unwrap()`
    ///  │ │
    ///  │ │ │← y.source_info.scope
    ///  │ │
    ///  │ │ │{ let y: u32 }
    ///  │ │ │
    ///  │ │ │← y.var_debug_info.source_info.scope
    ///  │ │ │← `y + 2`
    ///  │
    ///  │ │{ let x: u32 }
    ///  │ │← x.var_debug_info.source_info.scope
    ///  │ │← `drop(x)` // This accesses `x: u32`.
    /// ```
    pub source_info: SourceInfo,
}

/// Extra information about a some locals that's used for diagnostics and for
/// classifying variables into local variables, statics, etc, which is needed e.g.
/// for unsafety checking.
///
/// Not used for non-StaticRef temporaries, the return place, or anonymous
/// function parameters.
#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub enum LocalInfo<'tcx> {
    /// A user-defined local variable or function parameter
    ///
    /// The `BindingForm` is solely used for local diagnostics when generating
    /// warnings/errors when compiling the current crate, and therefore it need
    /// not be visible across crates.
    User(BindingForm<'tcx>),
    /// A temporary created that references the static with the given `DefId`.
    StaticRef { def_id: DefId, is_thread_local: bool },
    /// A temporary created that references the const with the given `DefId`
    ConstRef { def_id: DefId },
    /// A temporary created during the creation of an aggregate
    /// (e.g. a temporary for `foo` in `MyStruct { my_field: foo }`)
    AggregateTemp,
    /// A temporary created for evaluation of some subexpression of some block's tail expression
    /// (with no intervening statement context).
    // FIXME(matthewjasper) Don't store in this in `Body`
    BlockTailTemp(BlockTailInfo),
    /// A temporary created during the pass `Derefer` to avoid it's retagging
    DerefTemp,
    /// A temporary created for borrow checking.
    FakeBorrow,
    /// A local without anything interesting about it.
    Boring,
}

impl<'tcx> LocalDecl<'tcx> {
    pub fn local_info(&self) -> &LocalInfo<'tcx> {
        &self.local_info.as_ref().assert_crate_local()
    }

    /// Returns `true` only if local is a binding that can itself be
    /// made mutable via the addition of the `mut` keyword, namely
    /// something like the occurrences of `x` in:
    /// - `fn foo(x: Type) { ... }`,
    /// - `let x = ...`,
    /// - or `match ... { C(x) => ... }`
    pub fn can_be_made_mutable(&self) -> bool {
        matches!(
            self.local_info(),
            LocalInfo::User(
                BindingForm::Var(VarBindingForm {
                    binding_mode: ty::BindingMode::BindByValue(_),
                    opt_ty_info: _,
                    opt_match_place: _,
                    pat_span: _,
                }) | BindingForm::ImplicitSelf(ImplicitSelfKind::Imm),
            )
        )
    }

    /// Returns `true` if local is definitely not a `ref ident` or
    /// `ref mut ident` binding. (Such bindings cannot be made into
    /// mutable bindings, but the inverse does not necessarily hold).
    pub fn is_nonref_binding(&self) -> bool {
        matches!(
            self.local_info(),
            LocalInfo::User(
                BindingForm::Var(VarBindingForm {
                    binding_mode: ty::BindingMode::BindByValue(_),
                    opt_ty_info: _,
                    opt_match_place: _,
                    pat_span: _,
                }) | BindingForm::ImplicitSelf(_),
            )
        )
    }

    /// Returns `true` if this variable is a named variable or function
    /// parameter declared by the user.
    #[inline]
    pub fn is_user_variable(&self) -> bool {
        matches!(self.local_info(), LocalInfo::User(_))
    }

    /// Returns `true` if this is a reference to a variable bound in a `match`
    /// expression that is used to access said variable for the guard of the
    /// match arm.
    pub fn is_ref_for_guard(&self) -> bool {
        matches!(self.local_info(), LocalInfo::User(BindingForm::RefForGuard))
    }

    /// Returns `Some` if this is a reference to a static item that is used to
    /// access that static.
    pub fn is_ref_to_static(&self) -> bool {
        matches!(self.local_info(), LocalInfo::StaticRef { .. })
    }

    /// Returns `Some` if this is a reference to a thread-local static item that is used to
    /// access that static.
    pub fn is_ref_to_thread_local(&self) -> bool {
        match self.local_info() {
            LocalInfo::StaticRef { is_thread_local, .. } => *is_thread_local,
            _ => false,
        }
    }

    /// Returns `true` if this is a DerefTemp
    pub fn is_deref_temp(&self) -> bool {
        match self.local_info() {
            LocalInfo::DerefTemp => return true,
            _ => (),
        }
        return false;
    }

    /// Returns `true` is the local is from a compiler desugaring, e.g.,
    /// `__next` from a `for` loop.
    #[inline]
    pub fn from_compiler_desugaring(&self) -> bool {
        self.source_info.span.desugaring_kind().is_some()
    }

    /// Creates a new `LocalDecl` for a temporary: mutable, non-internal.
    #[inline]
    pub fn new(ty: Ty<'tcx>, span: Span) -> Self {
        Self::with_source_info(ty, SourceInfo::outermost(span))
    }

    /// Like `LocalDecl::new`, but takes a `SourceInfo` instead of a `Span`.
    #[inline]
    pub fn with_source_info(ty: Ty<'tcx>, source_info: SourceInfo) -> Self {
        LocalDecl {
            mutability: Mutability::Mut,
            local_info: ClearCrossCrate::Set(Box::new(LocalInfo::Boring)),
            internal: false,
            ty,
            user_ty: None,
            source_info,
        }
    }

    /// Converts `self` into same `LocalDecl` except tagged as internal.
    #[inline]
    pub fn internal(mut self) -> Self {
        self.internal = true;
        self
    }

    /// Converts `self` into same `LocalDecl` except tagged as immutable.
    #[inline]
    pub fn immutable(mut self) -> Self {
        self.mutability = Mutability::Not;
        self
    }
}

#[derive(Clone, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub enum VarDebugInfoContents<'tcx> {
    /// This `Place` only contains projection which satisfy `can_use_in_debuginfo`.
    Place(Place<'tcx>),
    Const(Constant<'tcx>),
}

impl<'tcx> Debug for VarDebugInfoContents<'tcx> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
        match self {
            VarDebugInfoContents::Const(c) => write!(fmt, "{c}"),
            VarDebugInfoContents::Place(p) => write!(fmt, "{p:?}"),
        }
    }
}

#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub struct VarDebugInfoFragment<'tcx> {
    /// Type of the original user variable.
    /// This cannot contain a union or an enum.
    pub ty: Ty<'tcx>,

    /// Where in the composite user variable this fragment is,
    /// represented as a "projection" into the composite variable.
    /// At lower levels, this corresponds to a byte/bit range.
    ///
    /// This can only contain `PlaceElem::Field`.
    // FIXME support this for `enum`s by either using DWARF's
    // more advanced control-flow features (unsupported by LLVM?)
    // to match on the discriminant, or by using custom type debuginfo
    // with non-overlapping variants for the composite variable.
    pub projection: Vec<PlaceElem<'tcx>>,
}

/// Debug information pertaining to a user variable.
#[derive(Clone, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub struct VarDebugInfo<'tcx> {
    pub name: Symbol,

    /// Source info of the user variable, including the scope
    /// within which the variable is visible (to debuginfo)
    /// (see `LocalDecl`'s `source_info` field for more details).
    pub source_info: SourceInfo,

    /// The user variable's data is split across several fragments,
    /// each described by a `VarDebugInfoFragment`.
    /// See DWARF 5's "2.6.1.2 Composite Location Descriptions"
    /// and LLVM's `DW_OP_LLVM_fragment` for more details on
    /// the underlying debuginfo feature this relies on.
    pub composite: Option<Box<VarDebugInfoFragment<'tcx>>>,

    /// Where the data for this user variable is to be found.
    pub value: VarDebugInfoContents<'tcx>,

    /// When present, indicates what argument number this variable is in the function that it
    /// originated from (starting from 1). Note, if MIR inlining is enabled, then this is the
    /// argument number in the original function before it was inlined.
    pub argument_index: Option<u16>,
}

///////////////////////////////////////////////////////////////////////////
// BasicBlock

rustc_index::newtype_index! {
    /// A node in the MIR [control-flow graph][CFG].
    ///
    /// There are no branches (e.g., `if`s, function calls, etc.) within a basic block, which makes
    /// it easier to do [data-flow analyses] and optimizations. Instead, branches are represented
    /// as an edge in a graph between basic blocks.
    ///
    /// Basic blocks consist of a series of [statements][Statement], ending with a
    /// [terminator][Terminator]. Basic blocks can have multiple predecessors and successors,
    /// however there is a MIR pass ([`CriticalCallEdges`]) that removes *critical edges*, which
    /// are edges that go from a multi-successor node to a multi-predecessor node. This pass is
    /// needed because some analyses require that there are no critical edges in the CFG.
    ///
    /// Note that this type is just an index into [`Body.basic_blocks`](Body::basic_blocks);
    /// the actual data that a basic block holds is in [`BasicBlockData`].
    ///
    /// Read more about basic blocks in the [rustc-dev-guide][guide-mir].
    ///
    /// [CFG]: https://rustc-dev-guide.rust-lang.org/appendix/background.html#cfg
    /// [data-flow analyses]:
    ///     https://rustc-dev-guide.rust-lang.org/appendix/background.html#what-is-a-dataflow-analysis
    /// [`CriticalCallEdges`]: ../../rustc_const_eval/transform/add_call_guards/enum.AddCallGuards.html#variant.CriticalCallEdges
    /// [guide-mir]: https://rustc-dev-guide.rust-lang.org/mir/
    #[derive(HashStable)]
    #[debug_format = "bb{}"]
    pub struct BasicBlock {
        const START_BLOCK = 0;
    }
}

impl BasicBlock {
    pub fn start_location(self) -> Location {
        Location { block: self, statement_index: 0 }
    }
}

///////////////////////////////////////////////////////////////////////////
// BasicBlockData

/// Data for a basic block, including a list of its statements.
///
/// See [`BasicBlock`] for documentation on what basic blocks are at a high level.
#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub struct BasicBlockData<'tcx> {
    /// List of statements in this block.
    pub statements: Vec<Statement<'tcx>>,

    /// Terminator for this block.
    ///
    /// N.B., this should generally ONLY be `None` during construction.
    /// Therefore, you should generally access it via the
    /// `terminator()` or `terminator_mut()` methods. The only
    /// exception is that certain passes, such as `simplify_cfg`, swap
    /// out the terminator temporarily with `None` while they continue
    /// to recurse over the set of basic blocks.
    pub terminator: Option<Terminator<'tcx>>,

    /// If true, this block lies on an unwind path. This is used
    /// during codegen where distinct kinds of basic blocks may be
    /// generated (particularly for MSVC cleanup). Unwind blocks must
    /// only branch to other unwind blocks.
    pub is_cleanup: bool,
}

impl<'tcx> BasicBlockData<'tcx> {
    pub fn new(terminator: Option<Terminator<'tcx>>) -> BasicBlockData<'tcx> {
        BasicBlockData { statements: vec![], terminator, is_cleanup: false }
    }

    /// Accessor for terminator.
    ///
    /// Terminator may not be None after construction of the basic block is complete. This accessor
    /// provides a convenient way to reach the terminator.
    #[inline]
    pub fn terminator(&self) -> &Terminator<'tcx> {
        self.terminator.as_ref().expect("invalid terminator state")
    }

    #[inline]
    pub fn terminator_mut(&mut self) -> &mut Terminator<'tcx> {
        self.terminator.as_mut().expect("invalid terminator state")
    }

    pub fn retain_statements<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Statement<'_>) -> bool,
    {
        for s in &mut self.statements {
            if !f(s) {
                s.make_nop();
            }
        }
    }

    pub fn expand_statements<F, I>(&mut self, mut f: F)
    where
        F: FnMut(&mut Statement<'tcx>) -> Option<I>,
        I: iter::TrustedLen<Item = Statement<'tcx>>,
    {
        // Gather all the iterators we'll need to splice in, and their positions.
        let mut splices: Vec<(usize, I)> = vec![];
        let mut extra_stmts = 0;
        for (i, s) in self.statements.iter_mut().enumerate() {
            if let Some(mut new_stmts) = f(s) {
                if let Some(first) = new_stmts.next() {
                    // We can already store the first new statement.
                    *s = first;

                    // Save the other statements for optimized splicing.
                    let remaining = new_stmts.size_hint().0;
                    if remaining > 0 {
                        splices.push((i + 1 + extra_stmts, new_stmts));
                        extra_stmts += remaining;
                    }
                } else {
                    s.make_nop();
                }
            }
        }

        // Splice in the new statements, from the end of the block.
        // FIXME(eddyb) This could be more efficient with a "gap buffer"
        // where a range of elements ("gap") is left uninitialized, with
        // splicing adding new elements to the end of that gap and moving
        // existing elements from before the gap to the end of the gap.
        // For now, this is safe code, emulating a gap but initializing it.
        let mut gap = self.statements.len()..self.statements.len() + extra_stmts;
        self.statements.resize(
            gap.end,
            Statement { source_info: SourceInfo::outermost(DUMMY_SP), kind: StatementKind::Nop },
        );
        for (splice_start, new_stmts) in splices.into_iter().rev() {
            let splice_end = splice_start + new_stmts.size_hint().0;
            while gap.end > splice_end {
                gap.start -= 1;
                gap.end -= 1;
                self.statements.swap(gap.start, gap.end);
            }
            self.statements.splice(splice_start..splice_end, new_stmts);
            gap.end = splice_start;
        }
    }

    pub fn visitable(&self, index: usize) -> &dyn MirVisitable<'tcx> {
        if index < self.statements.len() { &self.statements[index] } else { &self.terminator }
    }

    /// Does the block have no statements and an unreachable terminator?
    pub fn is_empty_unreachable(&self) -> bool {
        self.statements.is_empty() && matches!(self.terminator().kind, TerminatorKind::Unreachable)
    }
}

///////////////////////////////////////////////////////////////////////////
// Scopes

rustc_index::newtype_index! {
    #[derive(HashStable)]
    #[debug_format = "scope[{}]"]
    pub struct SourceScope {
        const OUTERMOST_SOURCE_SCOPE = 0;
    }
}

impl SourceScope {
    /// Finds the original HirId this MIR item came from.
    /// This is necessary after MIR optimizations, as otherwise we get a HirId
    /// from the function that was inlined instead of the function call site.
    pub fn lint_root(
        self,
        source_scopes: &IndexSlice<SourceScope, SourceScopeData<'_>>,
    ) -> Option<HirId> {
        let mut data = &source_scopes[self];
        // FIXME(oli-obk): we should be able to just walk the `inlined_parent_scope`, but it
        // does not work as I thought it would. Needs more investigation and documentation.
        while data.inlined.is_some() {
            trace!(?data);
            data = &source_scopes[data.parent_scope.unwrap()];
        }
        trace!(?data);
        match &data.local_data {
            ClearCrossCrate::Set(data) => Some(data.lint_root),
            ClearCrossCrate::Clear => None,
        }
    }

    /// The instance this source scope was inlined from, if any.
    #[inline]
    pub fn inlined_instance<'tcx>(
        self,
        source_scopes: &IndexSlice<SourceScope, SourceScopeData<'tcx>>,
    ) -> Option<ty::Instance<'tcx>> {
        let scope_data = &source_scopes[self];
        if let Some((inlined_instance, _)) = scope_data.inlined {
            Some(inlined_instance)
        } else if let Some(inlined_scope) = scope_data.inlined_parent_scope {
            Some(source_scopes[inlined_scope].inlined.unwrap().0)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub struct SourceScopeData<'tcx> {
    pub span: Span,
    pub parent_scope: Option<SourceScope>,

    /// Whether this scope is the root of a scope tree of another body,
    /// inlined into this body by the MIR inliner.
    /// `ty::Instance` is the callee, and the `Span` is the call site.
    pub inlined: Option<(ty::Instance<'tcx>, Span)>,

    /// Nearest (transitive) parent scope (if any) which is inlined.
    /// This is an optimization over walking up `parent_scope`
    /// until a scope with `inlined: Some(...)` is found.
    pub inlined_parent_scope: Option<SourceScope>,

    /// Crate-local information for this source scope, that can't (and
    /// needn't) be tracked across crates.
    pub local_data: ClearCrossCrate<SourceScopeLocalData>,
}

#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable)]
pub struct SourceScopeLocalData {
    /// An `HirId` with lint levels equivalent to this scope's lint levels.
    pub lint_root: hir::HirId,
    /// The unsafe block that contains this node.
    pub safety: Safety,
}

/// A collection of projections into user types.
///
/// They are projections because a binding can occur a part of a
/// parent pattern that has been ascribed a type.
///
/// It's a collection because there can be multiple type ascriptions on
/// the path from the root of the pattern down to the binding itself.
///
/// An example:
///
/// ```ignore (illustrative)
/// struct S<'a>((i32, &'a str), String);
/// let S((_, w): (i32, &'static str), _): S = ...;
/// //    ------  ^^^^^^^^^^^^^^^^^^^ (1)
/// //  ---------------------------------  ^ (2)
/// ```
///
/// The highlights labelled `(1)` show the subpattern `(_, w)` being
/// ascribed the type `(i32, &'static str)`.
///
/// The highlights labelled `(2)` show the whole pattern being
/// ascribed the type `S`.
///
/// In this example, when we descend to `w`, we will have built up the
/// following two projected types:
///
///   * base: `S`,                   projection: `(base.0).1`
///   * base: `(i32, &'static str)`, projection: `base.1`
///
/// The first will lead to the constraint `w: &'1 str` (for some
/// inferred region `'1`). The second will lead to the constraint `w:
/// &'static str`.
#[derive(Clone, Debug, TyEncodable, TyDecodable, HashStable, TypeFoldable, TypeVisitable)]
pub struct UserTypeProjections {
    pub contents: Vec<(UserTypeProjection, Span)>,
}

impl<'tcx> UserTypeProjections {
    pub fn none() -> Self {
        UserTypeProjections { contents: vec![] }
    }

    pub fn is_empty(&self) -> bool {
        self.contents.is_empty()
    }

    pub fn projections_and_spans(
        &self,
    ) -> impl Iterator<Item = &(UserTypeProjection, Span)> + ExactSizeIterator {
        self.contents.iter()
    }

    pub fn projections(&self) -> impl Iterator<Item = &UserTypeProjection> + ExactSizeIterator {
        self.contents.iter().map(|&(ref user_type, _span)| user_type)
    }

    pub fn push_projection(mut self, user_ty: &UserTypeProjection, span: Span) -> Self {
        self.contents.push((user_ty.clone(), span));
        self
    }

    fn map_projections(
        mut self,
        mut f: impl FnMut(UserTypeProjection) -> UserTypeProjection,
    ) -> Self {
        self.contents = self.contents.into_iter().map(|(proj, span)| (f(proj), span)).collect();
        self
    }

    pub fn index(self) -> Self {
        self.map_projections(|pat_ty_proj| pat_ty_proj.index())
    }

    pub fn subslice(self, from: u64, to: u64) -> Self {
        self.map_projections(|pat_ty_proj| pat_ty_proj.subslice(from, to))
    }

    pub fn deref(self) -> Self {
        self.map_projections(|pat_ty_proj| pat_ty_proj.deref())
    }

    pub fn leaf(self, field: FieldIdx) -> Self {
        self.map_projections(|pat_ty_proj| pat_ty_proj.leaf(field))
    }

    pub fn variant(
        self,
        adt_def: AdtDef<'tcx>,
        variant_index: VariantIdx,
        field_index: FieldIdx,
    ) -> Self {
        self.map_projections(|pat_ty_proj| pat_ty_proj.variant(adt_def, variant_index, field_index))
    }
}

/// Encodes the effect of a user-supplied type annotation on the
/// subcomponents of a pattern. The effect is determined by applying the
/// given list of projections to some underlying base type. Often,
/// the projection element list `projs` is empty, in which case this
/// directly encodes a type in `base`. But in the case of complex patterns with
/// subpatterns and bindings, we want to apply only a *part* of the type to a variable,
/// in which case the `projs` vector is used.
///
/// Examples:
///
/// * `let x: T = ...` -- here, the `projs` vector is empty.
///
/// * `let (x, _): T = ...` -- here, the `projs` vector would contain
///   `field[0]` (aka `.0`), indicating that the type of `s` is
///   determined by finding the type of the `.0` field from `T`.
#[derive(Clone, Debug, TyEncodable, TyDecodable, Hash, HashStable, PartialEq)]
#[derive(TypeFoldable, TypeVisitable)]
pub struct UserTypeProjection {
    pub base: UserTypeAnnotationIndex,
    pub projs: Vec<ProjectionKind>,
}

impl UserTypeProjection {
    pub(crate) fn index(mut self) -> Self {
        self.projs.push(ProjectionElem::Index(()));
        self
    }

    pub(crate) fn subslice(mut self, from: u64, to: u64) -> Self {
        self.projs.push(ProjectionElem::Subslice { from, to, from_end: true });
        self
    }

    pub(crate) fn deref(mut self) -> Self {
        self.projs.push(ProjectionElem::Deref);
        self
    }

    pub(crate) fn leaf(mut self, field: FieldIdx) -> Self {
        self.projs.push(ProjectionElem::Field(field, ()));
        self
    }

    pub(crate) fn variant(
        mut self,
        adt_def: AdtDef<'_>,
        variant_index: VariantIdx,
        field_index: FieldIdx,
    ) -> Self {
        self.projs.push(ProjectionElem::Downcast(
            Some(adt_def.variant(variant_index).name),
            variant_index,
        ));
        self.projs.push(ProjectionElem::Field(field_index, ()));
        self
    }
}

rustc_index::newtype_index! {
    #[derive(HashStable)]
    #[debug_format = "promoted[{}]"]
    pub struct Promoted {}
}

/// `Location` represents the position of the start of the statement; or, if
/// `statement_index` equals the number of statements, then the start of the
/// terminator.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, HashStable)]
pub struct Location {
    /// The block that the location is within.
    pub block: BasicBlock,

    pub statement_index: usize,
}

impl fmt::Debug for Location {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{:?}[{}]", self.block, self.statement_index)
    }
}

impl Location {
    pub const START: Location = Location { block: START_BLOCK, statement_index: 0 };

    /// Returns the location immediately after this one within the enclosing block.
    ///
    /// Note that if this location represents a terminator, then the
    /// resulting location would be out of bounds and invalid.
    pub fn successor_within_block(&self) -> Location {
        Location { block: self.block, statement_index: self.statement_index + 1 }
    }

    /// Returns `true` if `other` is earlier in the control flow graph than `self`.
    pub fn is_predecessor_of<'tcx>(&self, other: Location, body: &Body<'tcx>) -> bool {
        // If we are in the same block as the other location and are an earlier statement
        // then we are a predecessor of `other`.
        if self.block == other.block && self.statement_index < other.statement_index {
            return true;
        }

        let predecessors = body.basic_blocks.predecessors();

        // If we're in another block, then we want to check that block is a predecessor of `other`.
        let mut queue: Vec<BasicBlock> = predecessors[other.block].to_vec();
        let mut visited = FxHashSet::default();

        while let Some(block) = queue.pop() {
            // If we haven't visited this block before, then make sure we visit its predecessors.
            if visited.insert(block) {
                queue.extend(predecessors[block].iter().cloned());
            } else {
                continue;
            }

            // If we found the block that `self` is in, then we are a predecessor of `other` (since
            // we found that block by looking at the predecessors of `other`).
            if self.block == block {
                return true;
            }
        }

        false
    }

    pub fn dominates(&self, other: Location, dominators: &Dominators<BasicBlock>) -> bool {
        if self.block == other.block {
            self.statement_index <= other.statement_index
        } else {
            dominators.dominates(self.block, other.block)
        }
    }
}

// Some nodes are used a lot. Make sure they don't unintentionally get bigger.
#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
mod size_asserts {
    use super::*;
    use rustc_data_structures::static_assert_size;
    // tidy-alphabetical-start
    static_assert_size!(BasicBlockData<'_>, 136);
    static_assert_size!(LocalDecl<'_>, 40);
    static_assert_size!(SourceScopeData<'_>, 72);
    static_assert_size!(Statement<'_>, 32);
    static_assert_size!(StatementKind<'_>, 16);
    static_assert_size!(Terminator<'_>, 104);
    static_assert_size!(TerminatorKind<'_>, 88);
    static_assert_size!(VarDebugInfo<'_>, 88);
    // tidy-alphabetical-end
}
