//! HIR (previously known as descriptors) provides a high-level object oriented
//! access to Rust code.
//!
//! The principal difference between HIR and syntax trees is that HIR is bound
//! to a particular crate instance. That is, it has cfg flags and features
//! applied. So, the relation between syntax and HIR is many-to-one.
//!
//! HIR is the public API of the all of the compiler logic above syntax trees.
//! It is written in "OO" style. Each type is self contained (as in, it knows it's
//! parents and full context). It should be "clean code".
//!
//! `hir_*` crates are the implementation of the compiler logic.
//! They are written in "ECS" style, with relatively little abstractions.
//! Many types are not self-contained, and explicitly use local indexes, arenas, etc.
//!
//! `hir` is what insulates the "we don't know how to actually write an incremental compiler"
//! from the ide with completions, hovers, etc. It is a (soft, internal) boundary:
//! <https://www.tedinski.com/2018/02/06/system-boundaries.html>.

#![warn(rust_2018_idioms, unused_lifetimes, semicolon_in_expressions_from_macros)]
#![recursion_limit = "512"]

mod semantics;
mod source_analyzer;

mod from_id;
mod attrs;
mod has_source;

pub mod diagnostics;
pub mod db;
pub mod symbols;

mod display;

use std::{iter, ops::ControlFlow, sync::Arc};

use arrayvec::ArrayVec;
use base_db::{CrateDisplayName, CrateId, CrateOrigin, Edition, FileId, ProcMacroKind};
use either::Either;
use hir_def::{
    adt::{ReprData, VariantData},
    body::{BodyDiagnostic, SyntheticSyntax},
    expr::{BindingAnnotation, LabelId, Pat, PatId},
    generics::{TypeOrConstParamData, TypeParamProvenance},
    item_tree::ItemTreeNode,
    lang_item::LangItemTarget,
    nameres::{self, diagnostics::DefDiagnostic},
    per_ns::PerNs,
    resolver::{HasResolver, Resolver},
    src::HasSource as _,
    AdtId, AssocItemId, AssocItemLoc, AttrDefId, ConstId, ConstParamId, DefWithBodyId, EnumId,
    EnumVariantId, FunctionId, GenericDefId, HasModule, ImplId, ItemContainerId, LifetimeParamId,
    LocalEnumVariantId, LocalFieldId, Lookup, MacroExpander, MacroId, ModuleId, StaticId, StructId,
    TraitId, TypeAliasId, TypeOrConstParamId, TypeParamId, UnionId,
};
use hir_expand::{name::name, MacroCallKind};
use hir_ty::{
    all_super_traits, autoderef,
    consteval::{unknown_const_as_generic, ComputedExpr, ConstEvalError, ConstExt},
    diagnostics::BodyValidationDiagnostic,
    method_resolution::{self, TyFingerprint},
    primitive::UintTy,
    subst_prefix,
    traits::FnTrait,
    AliasTy, CallableDefId, CallableSig, Canonical, CanonicalVarKinds, Cast, ClosureId,
    GenericArgData, Interner, ParamKind, QuantifiedWhereClause, Scalar, Substitution,
    TraitEnvironment, TraitRefExt, Ty, TyBuilder, TyDefId, TyExt, TyKind, WhereClause,
};
use itertools::Itertools;
use nameres::diagnostics::DefDiagnosticKind;
use once_cell::unsync::Lazy;
use rustc_hash::FxHashSet;
use stdx::{impl_from, never};
use syntax::{
    ast::{self, Expr, HasAttrs as _, HasDocComments, HasName},
    AstNode, AstPtr, SmolStr, SyntaxNodePtr, TextRange, T,
};

use crate::db::{DefDatabase, HirDatabase};

pub use crate::{
    attrs::{HasAttrs, Namespace},
    diagnostics::{
        AnyDiagnostic, BreakOutsideOfLoop, InactiveCode, IncorrectCase, InvalidDeriveTarget,
        MacroError, MalformedDerive, MismatchedArgCount, MissingFields, MissingMatchArms,
        MissingUnsafe, NoSuchField, ReplaceFilterMapNextWithFindMap, TypeMismatch,
        UnimplementedBuiltinMacro, UnresolvedExternCrate, UnresolvedImport, UnresolvedMacroCall,
        UnresolvedModule, UnresolvedProcMacro,
    },
    has_source::HasSource,
    semantics::{PathResolution, Semantics, SemanticsScope, TypeInfo, VisibleTraits},
};

// Be careful with these re-exports.
//
// `hir` is the boundary between the compiler and the IDE. It should try hard to
// isolate the compiler from the ide, to allow the two to be refactored
// independently. Re-exporting something from the compiler is the sure way to
// breach the boundary.
//
// Generally, a refactoring which *removes* a name from this list is a good
// idea!
pub use {
    cfg::{CfgAtom, CfgExpr, CfgOptions},
    hir_def::{
        adt::StructKind,
        attr::{Attr, Attrs, AttrsWithOwner, Documentation},
        builtin_attr::AttributeTemplate,
        find_path::PrefixKind,
        import_map,
        nameres::ModuleSource,
        path::{ModPath, PathKind},
        type_ref::{Mutability, TypeRef},
        visibility::Visibility,
    },
    hir_expand::{
        name::{known, Name},
        ExpandResult, HirFileId, InFile, MacroFile, Origin,
    },
    hir_ty::display::HirDisplay,
};

// These are negative re-exports: pub using these names is forbidden, they
// should remain private to hir internals.
#[allow(unused)]
use {
    hir_def::path::Path,
    hir_expand::{hygiene::Hygiene, name::AsName},
};

/// hir::Crate describes a single crate. It's the main interface with which
/// a crate's dependencies interact. Mostly, it should be just a proxy for the
/// root module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crate {
    pub(crate) id: CrateId,
}

#[derive(Debug)]
pub struct CrateDependency {
    pub krate: Crate,
    pub name: Name,
}

impl Crate {
    pub fn origin(self, db: &dyn HirDatabase) -> CrateOrigin {
        db.crate_graph()[self.id].origin.clone()
    }

    pub fn is_builtin(self, db: &dyn HirDatabase) -> bool {
        matches!(self.origin(db), CrateOrigin::Lang(_))
    }

    pub fn dependencies(self, db: &dyn HirDatabase) -> Vec<CrateDependency> {
        db.crate_graph()[self.id]
            .dependencies
            .iter()
            .map(|dep| {
                let krate = Crate { id: dep.crate_id };
                let name = dep.as_name();
                CrateDependency { krate, name }
            })
            .collect()
    }

    pub fn reverse_dependencies(self, db: &dyn HirDatabase) -> Vec<Crate> {
        let crate_graph = db.crate_graph();
        crate_graph
            .iter()
            .filter(|&krate| {
                crate_graph[krate].dependencies.iter().any(|it| it.crate_id == self.id)
            })
            .map(|id| Crate { id })
            .collect()
    }

    pub fn transitive_reverse_dependencies(
        self,
        db: &dyn HirDatabase,
    ) -> impl Iterator<Item = Crate> {
        db.crate_graph().transitive_rev_deps(self.id).map(|id| Crate { id })
    }

    pub fn root_module(self, db: &dyn HirDatabase) -> Module {
        let def_map = db.crate_def_map(self.id);
        Module { id: def_map.module_id(def_map.root()) }
    }

    pub fn modules(self, db: &dyn HirDatabase) -> Vec<Module> {
        let def_map = db.crate_def_map(self.id);
        def_map.modules().map(|(id, _)| def_map.module_id(id).into()).collect()
    }

    pub fn root_file(self, db: &dyn HirDatabase) -> FileId {
        db.crate_graph()[self.id].root_file_id
    }

    pub fn edition(self, db: &dyn HirDatabase) -> Edition {
        db.crate_graph()[self.id].edition
    }

    pub fn version(self, db: &dyn HirDatabase) -> Option<String> {
        db.crate_graph()[self.id].version.clone()
    }

    pub fn display_name(self, db: &dyn HirDatabase) -> Option<CrateDisplayName> {
        db.crate_graph()[self.id].display_name.clone()
    }

    pub fn query_external_importables(
        self,
        db: &dyn DefDatabase,
        query: import_map::Query,
    ) -> impl Iterator<Item = Either<ModuleDef, Macro>> {
        let _p = profile::span("query_external_importables");
        import_map::search_dependencies(db, self.into(), query).into_iter().map(|item| {
            match ItemInNs::from(item) {
                ItemInNs::Types(mod_id) | ItemInNs::Values(mod_id) => Either::Left(mod_id),
                ItemInNs::Macros(mac_id) => Either::Right(mac_id),
            }
        })
    }

    pub fn all(db: &dyn HirDatabase) -> Vec<Crate> {
        db.crate_graph().iter().map(|id| Crate { id }).collect()
    }

    /// Try to get the root URL of the documentation of a crate.
    pub fn get_html_root_url(self: &Crate, db: &dyn HirDatabase) -> Option<String> {
        // Look for #![doc(html_root_url = "...")]
        let attrs = db.attrs(AttrDefId::ModuleId(self.root_module(db).into()));
        let doc_url = attrs.by_key("doc").find_string_value_in_tt("html_root_url");
        doc_url.map(|s| s.trim_matches('"').trim_end_matches('/').to_owned() + "/")
    }

    pub fn cfg(&self, db: &dyn HirDatabase) -> CfgOptions {
        db.crate_graph()[self.id].cfg_options.clone()
    }

    pub fn potential_cfg(&self, db: &dyn HirDatabase) -> CfgOptions {
        db.crate_graph()[self.id].potential_cfg_options.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    pub(crate) id: ModuleId,
}

/// The defs which can be visible in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleDef {
    Module(Module),
    Function(Function),
    Adt(Adt),
    // Can't be directly declared, but can be imported.
    Variant(Variant),
    Const(Const),
    Static(Static),
    Trait(Trait),
    TypeAlias(TypeAlias),
    BuiltinType(BuiltinType),
    Macro(Macro),
}
impl_from!(
    Module,
    Function,
    Adt(Struct, Enum, Union),
    Variant,
    Const,
    Static,
    Trait,
    TypeAlias,
    BuiltinType,
    Macro
    for ModuleDef
);

impl From<VariantDef> for ModuleDef {
    fn from(var: VariantDef) -> Self {
        match var {
            VariantDef::Struct(t) => Adt::from(t).into(),
            VariantDef::Union(t) => Adt::from(t).into(),
            VariantDef::Variant(t) => t.into(),
        }
    }
}

impl ModuleDef {
    pub fn module(self, db: &dyn HirDatabase) -> Option<Module> {
        match self {
            ModuleDef::Module(it) => it.parent(db),
            ModuleDef::Function(it) => Some(it.module(db)),
            ModuleDef::Adt(it) => Some(it.module(db)),
            ModuleDef::Variant(it) => Some(it.module(db)),
            ModuleDef::Const(it) => Some(it.module(db)),
            ModuleDef::Static(it) => Some(it.module(db)),
            ModuleDef::Trait(it) => Some(it.module(db)),
            ModuleDef::TypeAlias(it) => Some(it.module(db)),
            ModuleDef::Macro(it) => Some(it.module(db)),
            ModuleDef::BuiltinType(_) => None,
        }
    }

    pub fn canonical_path(&self, db: &dyn HirDatabase) -> Option<String> {
        let mut segments = vec![self.name(db)?];
        for m in self.module(db)?.path_to_root(db) {
            segments.extend(m.name(db))
        }
        segments.reverse();
        Some(segments.into_iter().join("::"))
    }

    pub fn canonical_module_path(
        &self,
        db: &dyn HirDatabase,
    ) -> Option<impl Iterator<Item = Module>> {
        self.module(db).map(|it| it.path_to_root(db).into_iter().rev())
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        let name = match self {
            ModuleDef::Module(it) => it.name(db)?,
            ModuleDef::Const(it) => it.name(db)?,
            ModuleDef::Adt(it) => it.name(db),
            ModuleDef::Trait(it) => it.name(db),
            ModuleDef::Function(it) => it.name(db),
            ModuleDef::Variant(it) => it.name(db),
            ModuleDef::TypeAlias(it) => it.name(db),
            ModuleDef::Static(it) => it.name(db),
            ModuleDef::Macro(it) => it.name(db),
            ModuleDef::BuiltinType(it) => it.name(),
        };
        Some(name)
    }

    pub fn diagnostics(self, db: &dyn HirDatabase) -> Vec<AnyDiagnostic> {
        let id = match self {
            ModuleDef::Adt(it) => match it {
                Adt::Struct(it) => it.id.into(),
                Adt::Enum(it) => it.id.into(),
                Adt::Union(it) => it.id.into(),
            },
            ModuleDef::Trait(it) => it.id.into(),
            ModuleDef::Function(it) => it.id.into(),
            ModuleDef::TypeAlias(it) => it.id.into(),
            ModuleDef::Module(it) => it.id.into(),
            ModuleDef::Const(it) => it.id.into(),
            ModuleDef::Static(it) => it.id.into(),
            ModuleDef::Variant(it) => {
                EnumVariantId { parent: it.parent.into(), local_id: it.id }.into()
            }
            ModuleDef::BuiltinType(_) | ModuleDef::Macro(_) => return Vec::new(),
        };

        let module = match self.module(db) {
            Some(it) => it,
            None => return Vec::new(),
        };

        let mut acc = Vec::new();

        match self.as_def_with_body() {
            Some(def) => {
                def.diagnostics(db, &mut acc);
            }
            None => {
                for diag in hir_ty::diagnostics::incorrect_case(db, module.id.krate(), id) {
                    acc.push(diag.into())
                }
            }
        }

        acc
    }

    pub fn as_def_with_body(self) -> Option<DefWithBody> {
        match self {
            ModuleDef::Function(it) => Some(it.into()),
            ModuleDef::Const(it) => Some(it.into()),
            ModuleDef::Static(it) => Some(it.into()),
            ModuleDef::Variant(it) => Some(it.into()),

            ModuleDef::Module(_)
            | ModuleDef::Adt(_)
            | ModuleDef::Trait(_)
            | ModuleDef::TypeAlias(_)
            | ModuleDef::Macro(_)
            | ModuleDef::BuiltinType(_) => None,
        }
    }

    pub fn attrs(&self, db: &dyn HirDatabase) -> Option<AttrsWithOwner> {
        Some(match self {
            ModuleDef::Module(it) => it.attrs(db),
            ModuleDef::Function(it) => it.attrs(db),
            ModuleDef::Adt(it) => it.attrs(db),
            ModuleDef::Variant(it) => it.attrs(db),
            ModuleDef::Const(it) => it.attrs(db),
            ModuleDef::Static(it) => it.attrs(db),
            ModuleDef::Trait(it) => it.attrs(db),
            ModuleDef::TypeAlias(it) => it.attrs(db),
            ModuleDef::Macro(it) => it.attrs(db),
            ModuleDef::BuiltinType(_) => return None,
        })
    }
}

impl HasVisibility for ModuleDef {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        match *self {
            ModuleDef::Module(it) => it.visibility(db),
            ModuleDef::Function(it) => it.visibility(db),
            ModuleDef::Adt(it) => it.visibility(db),
            ModuleDef::Const(it) => it.visibility(db),
            ModuleDef::Static(it) => it.visibility(db),
            ModuleDef::Trait(it) => it.visibility(db),
            ModuleDef::TypeAlias(it) => it.visibility(db),
            ModuleDef::Variant(it) => it.visibility(db),
            ModuleDef::Macro(it) => it.visibility(db),
            ModuleDef::BuiltinType(_) => Visibility::Public,
        }
    }
}

impl Module {
    /// Name of this module.
    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        let def_map = self.id.def_map(db.upcast());
        let parent = def_map[self.id.local_id].parent?;
        def_map[parent].children.iter().find_map(|(name, module_id)| {
            if *module_id == self.id.local_id {
                Some(name.clone())
            } else {
                None
            }
        })
    }

    /// Returns the crate this module is part of.
    pub fn krate(self) -> Crate {
        Crate { id: self.id.krate() }
    }

    /// Topmost parent of this module. Every module has a `crate_root`, but some
    /// might be missing `krate`. This can happen if a module's file is not included
    /// in the module tree of any target in `Cargo.toml`.
    pub fn crate_root(self, db: &dyn HirDatabase) -> Module {
        let def_map = db.crate_def_map(self.id.krate());
        Module { id: def_map.module_id(def_map.root()) }
    }

    pub fn is_crate_root(self, db: &dyn HirDatabase) -> bool {
        let def_map = db.crate_def_map(self.id.krate());
        def_map.root() == self.id.local_id
    }

    /// Iterates over all child modules.
    pub fn children(self, db: &dyn HirDatabase) -> impl Iterator<Item = Module> {
        let def_map = self.id.def_map(db.upcast());
        let children = def_map[self.id.local_id]
            .children
            .iter()
            .map(|(_, module_id)| Module { id: def_map.module_id(*module_id) })
            .collect::<Vec<_>>();
        children.into_iter()
    }

    /// Finds a parent module.
    pub fn parent(self, db: &dyn HirDatabase) -> Option<Module> {
        // FIXME: handle block expressions as modules (their parent is in a different DefMap)
        let def_map = self.id.def_map(db.upcast());
        let parent_id = def_map[self.id.local_id].parent?;
        Some(Module { id: def_map.module_id(parent_id) })
    }

    pub fn path_to_root(self, db: &dyn HirDatabase) -> Vec<Module> {
        let mut res = vec![self];
        let mut curr = self;
        while let Some(next) = curr.parent(db) {
            res.push(next);
            curr = next
        }
        res
    }

    /// Returns a `ModuleScope`: a set of items, visible in this module.
    pub fn scope(
        self,
        db: &dyn HirDatabase,
        visible_from: Option<Module>,
    ) -> Vec<(Name, ScopeDef)> {
        self.id.def_map(db.upcast())[self.id.local_id]
            .scope
            .entries()
            .filter_map(|(name, def)| {
                if let Some(m) = visible_from {
                    let filtered =
                        def.filter_visibility(|vis| vis.is_visible_from(db.upcast(), m.id));
                    if filtered.is_none() && !def.is_none() {
                        None
                    } else {
                        Some((name, filtered))
                    }
                } else {
                    Some((name, def))
                }
            })
            .flat_map(|(name, def)| {
                ScopeDef::all_items(def).into_iter().map(move |item| (name.clone(), item))
            })
            .collect()
    }

    /// Fills `acc` with the module's diagnostics.
    pub fn diagnostics(self, db: &dyn HirDatabase, acc: &mut Vec<AnyDiagnostic>) {
        let _p = profile::span("Module::diagnostics").detail(|| {
            format!("{:?}", self.name(db).map_or("<unknown>".into(), |name| name.to_string()))
        });
        let def_map = self.id.def_map(db.upcast());
        for diag in def_map.diagnostics() {
            if diag.in_module != self.id.local_id {
                // FIXME: This is accidentally quadratic.
                continue;
            }
            emit_def_diagnostic(db, acc, diag);
        }
        for decl in self.declarations(db) {
            match decl {
                ModuleDef::Module(m) => {
                    // Only add diagnostics from inline modules
                    if def_map[m.id.local_id].origin.is_inline() {
                        m.diagnostics(db, acc)
                    }
                }
                ModuleDef::Trait(t) => {
                    for diag in db.trait_data_with_diagnostics(t.id).1.iter() {
                        emit_def_diagnostic(db, acc, diag);
                    }
                    acc.extend(decl.diagnostics(db))
                }
                ModuleDef::Adt(Adt::Enum(e)) => {
                    for v in e.variants(db) {
                        acc.extend(ModuleDef::Variant(v).diagnostics(db));
                    }
                    acc.extend(decl.diagnostics(db))
                }
                _ => acc.extend(decl.diagnostics(db)),
            }
        }

        for impl_def in self.impl_defs(db) {
            for diag in db.impl_data_with_diagnostics(impl_def.id).1.iter() {
                emit_def_diagnostic(db, acc, diag);
            }

            for item in impl_def.items(db) {
                let def: DefWithBody = match item {
                    AssocItem::Function(it) => it.into(),
                    AssocItem::Const(it) => it.into(),
                    AssocItem::TypeAlias(_) => continue,
                };

                def.diagnostics(db, acc);
            }
        }
    }

    pub fn declarations(self, db: &dyn HirDatabase) -> Vec<ModuleDef> {
        let def_map = self.id.def_map(db.upcast());
        let scope = &def_map[self.id.local_id].scope;
        scope
            .declarations()
            .map(ModuleDef::from)
            .chain(scope.unnamed_consts().map(|id| ModuleDef::Const(Const::from(id))))
            .collect()
    }

    pub fn legacy_macros(self, db: &dyn HirDatabase) -> Vec<Macro> {
        let def_map = self.id.def_map(db.upcast());
        let scope = &def_map[self.id.local_id].scope;
        scope.legacy_macros().flat_map(|(_, it)| it).map(|&it| MacroId::from(it).into()).collect()
    }

    pub fn impl_defs(self, db: &dyn HirDatabase) -> Vec<Impl> {
        let def_map = self.id.def_map(db.upcast());
        def_map[self.id.local_id].scope.impls().map(Impl::from).collect()
    }

    /// Finds a path that can be used to refer to the given item from within
    /// this module, if possible.
    pub fn find_use_path(
        self,
        db: &dyn DefDatabase,
        item: impl Into<ItemInNs>,
        prefer_no_std: bool,
    ) -> Option<ModPath> {
        hir_def::find_path::find_path(db, item.into().into(), self.into(), prefer_no_std)
    }

    /// Finds a path that can be used to refer to the given item from within
    /// this module, if possible. This is used for returning import paths for use-statements.
    pub fn find_use_path_prefixed(
        self,
        db: &dyn DefDatabase,
        item: impl Into<ItemInNs>,
        prefix_kind: PrefixKind,
        prefer_no_std: bool,
    ) -> Option<ModPath> {
        hir_def::find_path::find_path_prefixed(
            db,
            item.into().into(),
            self.into(),
            prefix_kind,
            prefer_no_std,
        )
    }
}

fn emit_def_diagnostic(db: &dyn HirDatabase, acc: &mut Vec<AnyDiagnostic>, diag: &DefDiagnostic) {
    match &diag.kind {
        DefDiagnosticKind::UnresolvedModule { ast: declaration, candidates } => {
            let decl = declaration.to_node(db.upcast());
            acc.push(
                UnresolvedModule {
                    decl: InFile::new(declaration.file_id, AstPtr::new(&decl)),
                    candidates: candidates.clone(),
                }
                .into(),
            )
        }
        DefDiagnosticKind::UnresolvedExternCrate { ast } => {
            let item = ast.to_node(db.upcast());
            acc.push(
                UnresolvedExternCrate { decl: InFile::new(ast.file_id, AstPtr::new(&item)) }.into(),
            );
        }

        DefDiagnosticKind::UnresolvedImport { id, index } => {
            let file_id = id.file_id();
            let item_tree = id.item_tree(db.upcast());
            let import = &item_tree[id.value];

            let use_tree = import.use_tree_to_ast(db.upcast(), file_id, *index);
            acc.push(
                UnresolvedImport { decl: InFile::new(file_id, AstPtr::new(&use_tree)) }.into(),
            );
        }

        DefDiagnosticKind::UnconfiguredCode { ast, cfg, opts } => {
            let item = ast.to_node(db.upcast());
            acc.push(
                InactiveCode {
                    node: ast.with_value(AstPtr::new(&item).into()),
                    cfg: cfg.clone(),
                    opts: opts.clone(),
                }
                .into(),
            );
        }

        DefDiagnosticKind::UnresolvedProcMacro { ast, krate } => {
            let (node, precise_location, macro_name, kind) = precise_macro_call_location(ast, db);
            acc.push(
                UnresolvedProcMacro { node, precise_location, macro_name, kind, krate: *krate }
                    .into(),
            );
        }

        DefDiagnosticKind::UnresolvedMacroCall { ast, path } => {
            let (node, precise_location, _, _) = precise_macro_call_location(ast, db);
            acc.push(
                UnresolvedMacroCall {
                    macro_call: node,
                    precise_location,
                    path: path.clone(),
                    is_bang: matches!(ast, MacroCallKind::FnLike { .. }),
                }
                .into(),
            );
        }

        DefDiagnosticKind::MacroError { ast, message } => {
            let (node, precise_location, _, _) = precise_macro_call_location(ast, db);
            acc.push(MacroError { node, precise_location, message: message.clone() }.into());
        }

        DefDiagnosticKind::UnimplementedBuiltinMacro { ast } => {
            let node = ast.to_node(db.upcast());
            // Must have a name, otherwise we wouldn't emit it.
            let name = node.name().expect("unimplemented builtin macro with no name");
            acc.push(
                UnimplementedBuiltinMacro {
                    node: ast.with_value(SyntaxNodePtr::from(AstPtr::new(&name))),
                }
                .into(),
            );
        }
        DefDiagnosticKind::InvalidDeriveTarget { ast, id } => {
            let node = ast.to_node(db.upcast());
            let derive = node.attrs().nth(*id as usize);
            match derive {
                Some(derive) => {
                    acc.push(
                        InvalidDeriveTarget {
                            node: ast.with_value(SyntaxNodePtr::from(AstPtr::new(&derive))),
                        }
                        .into(),
                    );
                }
                None => stdx::never!("derive diagnostic on item without derive attribute"),
            }
        }
        DefDiagnosticKind::MalformedDerive { ast, id } => {
            let node = ast.to_node(db.upcast());
            let derive = node.attrs().nth(*id as usize);
            match derive {
                Some(derive) => {
                    acc.push(
                        MalformedDerive {
                            node: ast.with_value(SyntaxNodePtr::from(AstPtr::new(&derive))),
                        }
                        .into(),
                    );
                }
                None => stdx::never!("derive diagnostic on item without derive attribute"),
            }
        }
    }
}

fn precise_macro_call_location(
    ast: &MacroCallKind,
    db: &dyn HirDatabase,
) -> (InFile<SyntaxNodePtr>, Option<TextRange>, Option<String>, MacroKind) {
    // FIXME: maaybe we actually want slightly different ranges for the different macro diagnostics
    // - e.g. the full attribute for macro errors, but only the name for name resolution
    match ast {
        MacroCallKind::FnLike { ast_id, .. } => {
            let node = ast_id.to_node(db.upcast());
            (
                ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&node))),
                node.path()
                    .and_then(|it| it.segment())
                    .and_then(|it| it.name_ref())
                    .map(|it| it.syntax().text_range()),
                node.path().and_then(|it| it.segment()).map(|it| it.to_string()),
                MacroKind::ProcMacro,
            )
        }
        MacroCallKind::Derive { ast_id, derive_attr_index, derive_index } => {
            let node = ast_id.to_node(db.upcast());
            // Compute the precise location of the macro name's token in the derive
            // list.
            let token = (|| {
                let derive_attr = node
                    .doc_comments_and_attrs()
                    .nth(*derive_attr_index as usize)
                    .and_then(Either::left)?;
                let token_tree = derive_attr.meta()?.token_tree()?;
                let group_by = token_tree
                    .syntax()
                    .children_with_tokens()
                    .filter_map(|elem| match elem {
                        syntax::NodeOrToken::Token(tok) => Some(tok),
                        _ => None,
                    })
                    .group_by(|t| t.kind() == T![,]);
                let (_, mut group) = group_by
                    .into_iter()
                    .filter(|&(comma, _)| !comma)
                    .nth(*derive_index as usize)?;
                group.find(|t| t.kind() == T![ident])
            })();
            (
                ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&node))),
                token.as_ref().map(|tok| tok.text_range()),
                token.as_ref().map(ToString::to_string),
                MacroKind::Derive,
            )
        }
        MacroCallKind::Attr { ast_id, invoc_attr_index, .. } => {
            let node = ast_id.to_node(db.upcast());
            let attr = node
                .doc_comments_and_attrs()
                .nth((*invoc_attr_index) as usize)
                .and_then(Either::left)
                .unwrap_or_else(|| panic!("cannot find attribute #{}", invoc_attr_index));

            (
                ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&attr))),
                Some(attr.syntax().text_range()),
                attr.path()
                    .and_then(|path| path.segment())
                    .and_then(|seg| seg.name_ref())
                    .as_ref()
                    .map(ToString::to_string),
                MacroKind::Attr,
            )
        }
    }
}

impl HasVisibility for Module {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let def_map = self.id.def_map(db.upcast());
        let module_data = &def_map[self.id.local_id];
        module_data.visibility
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub(crate) parent: VariantDef,
    pub(crate) id: LocalFieldId,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FieldSource {
    Named(ast::RecordField),
    Pos(ast::TupleField),
}

impl Field {
    pub fn name(&self, db: &dyn HirDatabase) -> Name {
        self.parent.variant_data(db).fields()[self.id].name.clone()
    }

    /// Returns the type as in the signature of the struct (i.e., with
    /// placeholder types for type parameters). Only use this in the context of
    /// the field definition.
    pub fn ty(&self, db: &dyn HirDatabase) -> Type {
        let var_id = self.parent.into();
        let generic_def_id: GenericDefId = match self.parent {
            VariantDef::Struct(it) => it.id.into(),
            VariantDef::Union(it) => it.id.into(),
            VariantDef::Variant(it) => it.parent.id.into(),
        };
        let substs = TyBuilder::placeholder_subst(db, generic_def_id);
        let ty = db.field_types(var_id)[self.id].clone().substitute(Interner, &substs);
        Type::new(db, var_id, ty)
    }

    pub fn parent_def(&self, _db: &dyn HirDatabase) -> VariantDef {
        self.parent
    }
}

impl HasVisibility for Field {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let variant_data = self.parent.variant_data(db);
        let visibility = &variant_data.fields()[self.id].visibility;
        let parent_id: hir_def::VariantId = self.parent.into();
        visibility.resolve(db.upcast(), &parent_id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Struct {
    pub(crate) id: StructId,
}

impl Struct {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.struct_data(self.id).name.clone()
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        db.struct_data(self.id)
            .variant_data
            .fields()
            .iter()
            .map(|(id, _)| Field { parent: self.into(), id })
            .collect()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id)
    }

    pub fn repr(self, db: &dyn HirDatabase) -> Option<ReprData> {
        db.struct_data(self.id).repr.clone()
    }

    pub fn kind(self, db: &dyn HirDatabase) -> StructKind {
        self.variant_data(db).kind()
    }

    fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        db.struct_data(self.id).variant_data.clone()
    }
}

impl HasVisibility for Struct {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.struct_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Union {
    pub(crate) id: UnionId,
}

impl Union {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.union_data(self.id).name.clone()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id)
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        db.union_data(self.id)
            .variant_data
            .fields()
            .iter()
            .map(|(id, _)| Field { parent: self.into(), id })
            .collect()
    }

    fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        db.union_data(self.id).variant_data.clone()
    }
}

impl HasVisibility for Union {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.union_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enum {
    pub(crate) id: EnumId,
}

impl Enum {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.enum_data(self.id).name.clone()
    }

    pub fn variants(self, db: &dyn HirDatabase) -> Vec<Variant> {
        db.enum_data(self.id).variants.iter().map(|(id, _)| Variant { parent: self, id }).collect()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id)
    }

    /// The type of the enum variant bodies.
    pub fn variant_body_ty(self, db: &dyn HirDatabase) -> Type {
        Type::new_for_crate(
            self.id.lookup(db.upcast()).container.krate(),
            TyBuilder::builtin(match db.enum_data(self.id).variant_body_type() {
                Either::Left(builtin) => hir_def::builtin_type::BuiltinType::Int(builtin),
                Either::Right(builtin) => hir_def::builtin_type::BuiltinType::Uint(builtin),
            }),
        )
    }

    pub fn is_data_carrying(self, db: &dyn HirDatabase) -> bool {
        self.variants(db).iter().any(|v| !matches!(v.kind(db), StructKind::Unit))
    }
}

impl HasVisibility for Enum {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.enum_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

impl From<&Variant> for DefWithBodyId {
    fn from(&v: &Variant) -> Self {
        DefWithBodyId::VariantId(v.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Variant {
    pub(crate) parent: Enum,
    pub(crate) id: LocalEnumVariantId,
}

impl Variant {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.parent.module(db)
    }

    pub fn parent_enum(self, _db: &dyn HirDatabase) -> Enum {
        self.parent
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.enum_data(self.parent.id).variants[self.id].name.clone()
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        self.variant_data(db)
            .fields()
            .iter()
            .map(|(id, _)| Field { parent: self.into(), id })
            .collect()
    }

    pub fn kind(self, db: &dyn HirDatabase) -> StructKind {
        self.variant_data(db).kind()
    }

    pub(crate) fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        db.enum_data(self.parent.id).variants[self.id].variant_data.clone()
    }

    pub fn value(self, db: &dyn HirDatabase) -> Option<Expr> {
        self.source(db)?.value.expr()
    }

    pub fn eval(self, db: &dyn HirDatabase) -> Result<ComputedExpr, ConstEvalError> {
        db.const_eval_variant(self.into())
    }
}

/// Variants inherit visibility from the parent enum.
impl HasVisibility for Variant {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        self.parent_enum(db).visibility(db)
    }
}

/// A Data Type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adt {
    Struct(Struct),
    Union(Union),
    Enum(Enum),
}
impl_from!(Struct, Union, Enum for Adt);

impl Adt {
    pub fn has_non_default_type_params(self, db: &dyn HirDatabase) -> bool {
        let subst = db.generic_defaults(self.into());
        subst.iter().any(|ty| match ty.skip_binders().data(Interner) {
            GenericArgData::Ty(x) => x.is_unknown(),
            _ => false,
        })
    }

    /// Turns this ADT into a type. Any type parameters of the ADT will be
    /// turned into unknown types, which is good for e.g. finding the most
    /// general set of completions, but will not look very nice when printed.
    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let id = AdtId::from(self);
        Type::from_def(db, id)
    }

    /// Turns this ADT into a type with the given type parameters. This isn't
    /// the greatest API, FIXME find a better one.
    pub fn ty_with_args(self, db: &dyn HirDatabase, args: &[Type]) -> Type {
        let id = AdtId::from(self);
        let mut it = args.iter().map(|t| t.ty.clone());
        let ty = TyBuilder::def_ty(db, id.into())
            .fill(|x| {
                let r = it.next().unwrap_or_else(|| TyKind::Error.intern(Interner));
                match x {
                    ParamKind::Type => GenericArgData::Ty(r).intern(Interner),
                    ParamKind::Const(ty) => unknown_const_as_generic(ty.clone()),
                }
            })
            .build();
        Type::new(db, id, ty)
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            Adt::Struct(s) => s.module(db),
            Adt::Union(s) => s.module(db),
            Adt::Enum(e) => e.module(db),
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        match self {
            Adt::Struct(s) => s.name(db),
            Adt::Union(u) => u.name(db),
            Adt::Enum(e) => e.name(db),
        }
    }

    pub fn as_enum(&self) -> Option<Enum> {
        if let Self::Enum(v) = self {
            Some(*v)
        } else {
            None
        }
    }
}

impl HasVisibility for Adt {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        match self {
            Adt::Struct(it) => it.visibility(db),
            Adt::Union(it) => it.visibility(db),
            Adt::Enum(it) => it.visibility(db),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VariantDef {
    Struct(Struct),
    Union(Union),
    Variant(Variant),
}
impl_from!(Struct, Union, Variant for VariantDef);

impl VariantDef {
    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        match self {
            VariantDef::Struct(it) => it.fields(db),
            VariantDef::Union(it) => it.fields(db),
            VariantDef::Variant(it) => it.fields(db),
        }
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            VariantDef::Struct(it) => it.module(db),
            VariantDef::Union(it) => it.module(db),
            VariantDef::Variant(it) => it.module(db),
        }
    }

    pub fn name(&self, db: &dyn HirDatabase) -> Name {
        match self {
            VariantDef::Struct(s) => s.name(db),
            VariantDef::Union(u) => u.name(db),
            VariantDef::Variant(e) => e.name(db),
        }
    }

    pub(crate) fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        match self {
            VariantDef::Struct(it) => it.variant_data(db),
            VariantDef::Union(it) => it.variant_data(db),
            VariantDef::Variant(it) => it.variant_data(db),
        }
    }
}

/// The defs which have a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBody {
    Function(Function),
    Static(Static),
    Const(Const),
    Variant(Variant),
}
impl_from!(Function, Const, Static, Variant for DefWithBody);

impl DefWithBody {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            DefWithBody::Const(c) => c.module(db),
            DefWithBody::Function(f) => f.module(db),
            DefWithBody::Static(s) => s.module(db),
            DefWithBody::Variant(v) => v.module(db),
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        match self {
            DefWithBody::Function(f) => Some(f.name(db)),
            DefWithBody::Static(s) => Some(s.name(db)),
            DefWithBody::Const(c) => c.name(db),
            DefWithBody::Variant(v) => Some(v.name(db)),
        }
    }

    /// Returns the type this def's body has to evaluate to.
    pub fn body_type(self, db: &dyn HirDatabase) -> Type {
        match self {
            DefWithBody::Function(it) => it.ret_type(db),
            DefWithBody::Static(it) => it.ty(db),
            DefWithBody::Const(it) => it.ty(db),
            DefWithBody::Variant(it) => it.parent.variant_body_ty(db),
        }
    }

    fn id(&self) -> DefWithBodyId {
        match self {
            DefWithBody::Function(it) => it.id.into(),
            DefWithBody::Static(it) => it.id.into(),
            DefWithBody::Const(it) => it.id.into(),
            DefWithBody::Variant(it) => it.into(),
        }
    }

    /// A textual representation of the HIR of this def's body for debugging purposes.
    pub fn debug_hir(self, db: &dyn HirDatabase) -> String {
        let body = db.body(self.id());
        body.pretty_print(db.upcast(), self.id())
    }

    pub fn diagnostics(self, db: &dyn HirDatabase, acc: &mut Vec<AnyDiagnostic>) {
        let krate = self.module(db).id.krate();

        let (body, source_map) = db.body_with_source_map(self.into());

        for (_, def_map) in body.blocks(db.upcast()) {
            for diag in def_map.diagnostics() {
                emit_def_diagnostic(db, acc, diag);
            }
        }

        for diag in source_map.diagnostics() {
            match diag {
                BodyDiagnostic::InactiveCode { node, cfg, opts } => acc.push(
                    InactiveCode { node: node.clone(), cfg: cfg.clone(), opts: opts.clone() }
                        .into(),
                ),
                BodyDiagnostic::MacroError { node, message } => acc.push(
                    MacroError {
                        node: node.clone().map(|it| it.into()),
                        precise_location: None,
                        message: message.to_string(),
                    }
                    .into(),
                ),
                BodyDiagnostic::UnresolvedProcMacro { node, krate } => acc.push(
                    UnresolvedProcMacro {
                        node: node.clone().map(|it| it.into()),
                        precise_location: None,
                        macro_name: None,
                        kind: MacroKind::ProcMacro,
                        krate: *krate,
                    }
                    .into(),
                ),
                BodyDiagnostic::UnresolvedMacroCall { node, path } => acc.push(
                    UnresolvedMacroCall {
                        macro_call: node.clone().map(|ast_ptr| ast_ptr.into()),
                        precise_location: None,
                        path: path.clone(),
                        is_bang: true,
                    }
                    .into(),
                ),
            }
        }

        let infer = db.infer(self.into());
        let source_map = Lazy::new(|| db.body_with_source_map(self.into()).1);
        for d in &infer.diagnostics {
            match d {
                hir_ty::InferenceDiagnostic::NoSuchField { expr } => {
                    let field = source_map.field_syntax(*expr);
                    acc.push(NoSuchField { field }.into())
                }
                &hir_ty::InferenceDiagnostic::BreakOutsideOfLoop { expr, is_break } => {
                    let expr = source_map
                        .expr_syntax(expr)
                        .expect("break outside of loop in synthetic syntax");
                    acc.push(BreakOutsideOfLoop { expr, is_break }.into())
                }
                hir_ty::InferenceDiagnostic::MismatchedArgCount { call_expr, expected, found } => {
                    match source_map.expr_syntax(*call_expr) {
                        Ok(source_ptr) => acc.push(
                            MismatchedArgCount {
                                call_expr: source_ptr,
                                expected: *expected,
                                found: *found,
                            }
                            .into(),
                        ),
                        Err(SyntheticSyntax) => (),
                    }
                }
            }
        }
        for (expr, mismatch) in infer.expr_type_mismatches() {
            let expr = match source_map.expr_syntax(expr) {
                Ok(expr) => expr,
                Err(SyntheticSyntax) => continue,
            };
            acc.push(
                TypeMismatch {
                    expr,
                    expected: Type::new(db, DefWithBodyId::from(self), mismatch.expected.clone()),
                    actual: Type::new(db, DefWithBodyId::from(self), mismatch.actual.clone()),
                }
                .into(),
            );
        }

        for expr in hir_ty::diagnostics::missing_unsafe(db, self.into()) {
            match source_map.expr_syntax(expr) {
                Ok(expr) => acc.push(MissingUnsafe { expr }.into()),
                Err(SyntheticSyntax) => {
                    // FIXME: Here and eslwhere in this file, the `expr` was
                    // desugared, report or assert that this doesn't happen.
                }
            }
        }

        for diagnostic in BodyValidationDiagnostic::collect(db, self.into()) {
            match diagnostic {
                BodyValidationDiagnostic::RecordMissingFields {
                    record,
                    variant,
                    missed_fields,
                } => {
                    let variant_data = variant.variant_data(db.upcast());
                    let missed_fields = missed_fields
                        .into_iter()
                        .map(|idx| variant_data.fields()[idx].name.clone())
                        .collect();

                    match record {
                        Either::Left(record_expr) => match source_map.expr_syntax(record_expr) {
                            Ok(source_ptr) => {
                                let root = source_ptr.file_syntax(db.upcast());
                                if let ast::Expr::RecordExpr(record_expr) =
                                    &source_ptr.value.to_node(&root)
                                {
                                    if record_expr.record_expr_field_list().is_some() {
                                        acc.push(
                                            MissingFields {
                                                file: source_ptr.file_id,
                                                field_list_parent: Either::Left(AstPtr::new(
                                                    record_expr,
                                                )),
                                                field_list_parent_path: record_expr
                                                    .path()
                                                    .map(|path| AstPtr::new(&path)),
                                                missed_fields,
                                            }
                                            .into(),
                                        )
                                    }
                                }
                            }
                            Err(SyntheticSyntax) => (),
                        },
                        Either::Right(record_pat) => match source_map.pat_syntax(record_pat) {
                            Ok(source_ptr) => {
                                if let Some(expr) = source_ptr.value.as_ref().left() {
                                    let root = source_ptr.file_syntax(db.upcast());
                                    if let ast::Pat::RecordPat(record_pat) = expr.to_node(&root) {
                                        if record_pat.record_pat_field_list().is_some() {
                                            acc.push(
                                                MissingFields {
                                                    file: source_ptr.file_id,
                                                    field_list_parent: Either::Right(AstPtr::new(
                                                        &record_pat,
                                                    )),
                                                    field_list_parent_path: record_pat
                                                        .path()
                                                        .map(|path| AstPtr::new(&path)),
                                                    missed_fields,
                                                }
                                                .into(),
                                            )
                                        }
                                    }
                                }
                            }
                            Err(SyntheticSyntax) => (),
                        },
                    }
                }
                BodyValidationDiagnostic::ReplaceFilterMapNextWithFindMap { method_call_expr } => {
                    if let Ok(next_source_ptr) = source_map.expr_syntax(method_call_expr) {
                        acc.push(
                            ReplaceFilterMapNextWithFindMap {
                                file: next_source_ptr.file_id,
                                next_expr: next_source_ptr.value,
                            }
                            .into(),
                        );
                    }
                }
                BodyValidationDiagnostic::MissingMatchArms { match_expr, uncovered_patterns } => {
                    match source_map.expr_syntax(match_expr) {
                        Ok(source_ptr) => {
                            let root = source_ptr.file_syntax(db.upcast());
                            if let ast::Expr::MatchExpr(match_expr) =
                                &source_ptr.value.to_node(&root)
                            {
                                if let Some(match_expr) = match_expr.expr() {
                                    acc.push(
                                        MissingMatchArms {
                                            file: source_ptr.file_id,
                                            match_expr: AstPtr::new(&match_expr),
                                            uncovered_patterns,
                                        }
                                        .into(),
                                    );
                                }
                            }
                        }
                        Err(SyntheticSyntax) => (),
                    }
                }
            }
        }

        let def: ModuleDef = match self {
            DefWithBody::Function(it) => it.into(),
            DefWithBody::Static(it) => it.into(),
            DefWithBody::Const(it) => it.into(),
            DefWithBody::Variant(it) => it.into(),
        };
        for diag in hir_ty::diagnostics::incorrect_case(db, krate, def.into()) {
            acc.push(diag.into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function {
    pub(crate) id: FunctionId,
}

impl Function {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.lookup(db.upcast()).module(db.upcast()).into()
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.function_data(self.id).name.clone()
    }

    /// Get this function's return type
    pub fn ret_type(self, db: &dyn HirDatabase) -> Type {
        let resolver = self.id.resolver(db.upcast());
        let substs = TyBuilder::placeholder_subst(db, self.id);
        let callable_sig = db.callable_item_signature(self.id.into()).substitute(Interner, &substs);
        let ty = callable_sig.ret().clone();
        Type::new_with_resolver_inner(db, &resolver, ty)
    }

    pub fn async_ret_type(self, db: &dyn HirDatabase) -> Option<Type> {
        if !self.is_async(db) {
            return None;
        }
        let resolver = self.id.resolver(db.upcast());
        let substs = TyBuilder::placeholder_subst(db, self.id);
        let callable_sig = db.callable_item_signature(self.id.into()).substitute(Interner, &substs);
        let ret_ty = callable_sig.ret().clone();
        for pred in ret_ty.impl_trait_bounds(db).into_iter().flatten() {
            if let WhereClause::AliasEq(output_eq) = pred.into_value_and_skipped_binders().0 {
                return Type::new_with_resolver_inner(db, &resolver, output_eq.ty).into();
            }
        }
        never!("Async fn ret_type should be impl Future");
        None
    }

    pub fn has_self_param(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).has_self_param()
    }

    pub fn self_param(self, db: &dyn HirDatabase) -> Option<SelfParam> {
        self.has_self_param(db).then(|| SelfParam { func: self.id })
    }

    pub fn assoc_fn_params(self, db: &dyn HirDatabase) -> Vec<Param> {
        let environment = db.trait_environment(self.id.into());
        let substs = TyBuilder::placeholder_subst(db, self.id);
        let callable_sig = db.callable_item_signature(self.id.into()).substitute(Interner, &substs);
        callable_sig
            .params()
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let ty = Type { env: environment.clone(), ty: ty.clone() };
                Param { func: self, ty, idx }
            })
            .collect()
    }

    pub fn method_params(self, db: &dyn HirDatabase) -> Option<Vec<Param>> {
        if self.self_param(db).is_none() {
            return None;
        }
        Some(self.params_without_self(db))
    }

    pub fn params_without_self(self, db: &dyn HirDatabase) -> Vec<Param> {
        let environment = db.trait_environment(self.id.into());
        let substs = TyBuilder::placeholder_subst(db, self.id);
        let callable_sig = db.callable_item_signature(self.id.into()).substitute(Interner, &substs);
        let skip = if db.function_data(self.id).has_self_param() { 1 } else { 0 };
        callable_sig
            .params()
            .iter()
            .enumerate()
            .skip(skip)
            .map(|(idx, ty)| {
                let ty = Type { env: environment.clone(), ty: ty.clone() };
                Param { func: self, ty, idx }
            })
            .collect()
    }

    pub fn is_const(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).has_const_kw()
    }

    pub fn is_async(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).has_async_kw()
    }

    pub fn is_unsafe_to_call(self, db: &dyn HirDatabase) -> bool {
        hir_ty::is_fn_unsafe_to_call(db, self.id)
    }

    /// Whether this function declaration has a definition.
    ///
    /// This is false in the case of required (not provided) trait methods.
    pub fn has_body(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).has_body()
    }

    pub fn as_proc_macro(self, db: &dyn HirDatabase) -> Option<Macro> {
        let function_data = db.function_data(self.id);
        let attrs = &function_data.attrs;
        // FIXME: Store this in FunctionData flags?
        if !(attrs.is_proc_macro()
            || attrs.is_proc_macro_attribute()
            || attrs.is_proc_macro_derive())
        {
            return None;
        }
        let loc = self.id.lookup(db.upcast());
        let def_map = db.crate_def_map(loc.krate(db).into());
        def_map.fn_as_proc_macro(self.id).map(|id| Macro { id: id.into() })
    }
}

// Note: logically, this belongs to `hir_ty`, but we are not using it there yet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Shared,
    Exclusive,
    Owned,
}

impl From<hir_ty::Mutability> for Access {
    fn from(mutability: hir_ty::Mutability) -> Access {
        match mutability {
            hir_ty::Mutability::Not => Access::Shared,
            hir_ty::Mutability::Mut => Access::Exclusive,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    func: Function,
    /// The index in parameter list, including self parameter.
    idx: usize,
    ty: Type,
}

impl Param {
    pub fn ty(&self) -> &Type {
        &self.ty
    }

    pub fn name(&self, db: &dyn HirDatabase) -> Option<Name> {
        db.function_data(self.func.id).params[self.idx].0.clone()
    }

    pub fn as_local(&self, db: &dyn HirDatabase) -> Option<Local> {
        let parent = DefWithBodyId::FunctionId(self.func.into());
        let body = db.body(parent);
        let pat_id = body.params[self.idx];
        if let Pat::Bind { .. } = &body[pat_id] {
            Some(Local { parent, pat_id: body.params[self.idx] })
        } else {
            None
        }
    }

    pub fn pattern_source(&self, db: &dyn HirDatabase) -> Option<ast::Pat> {
        self.source(db).and_then(|p| p.value.pat())
    }

    pub fn source(&self, db: &dyn HirDatabase) -> Option<InFile<ast::Param>> {
        let InFile { file_id, value } = self.func.source(db)?;
        let params = value.param_list()?;
        if params.self_param().is_some() {
            params.params().nth(self.idx.checked_sub(1)?)
        } else {
            params.params().nth(self.idx)
        }
        .map(|value| InFile { file_id, value })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelfParam {
    func: FunctionId,
}

impl SelfParam {
    pub fn access(self, db: &dyn HirDatabase) -> Access {
        let func_data = db.function_data(self.func);
        func_data
            .params
            .first()
            .map(|(_, param)| match &**param {
                TypeRef::Reference(.., mutability) => match mutability {
                    hir_def::type_ref::Mutability::Shared => Access::Shared,
                    hir_def::type_ref::Mutability::Mut => Access::Exclusive,
                },
                _ => Access::Owned,
            })
            .unwrap_or(Access::Owned)
    }

    pub fn display(self, db: &dyn HirDatabase) -> &'static str {
        match self.access(db) {
            Access::Shared => "&self",
            Access::Exclusive => "&mut self",
            Access::Owned => "self",
        }
    }

    pub fn source(&self, db: &dyn HirDatabase) -> Option<InFile<ast::SelfParam>> {
        let InFile { file_id, value } = Function::from(self.func).source(db)?;
        value
            .param_list()
            .and_then(|params| params.self_param())
            .map(|value| InFile { file_id, value })
    }

    pub fn ty(&self, db: &dyn HirDatabase) -> Type {
        let substs = TyBuilder::placeholder_subst(db, self.func);
        let callable_sig =
            db.callable_item_signature(self.func.into()).substitute(Interner, &substs);
        let environment = db.trait_environment(self.func.into());
        let ty = callable_sig.params()[0].clone();
        Type { env: environment, ty }
    }
}

impl HasVisibility for Function {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.function_visibility(self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Const {
    pub(crate) id: ConstId,
}

impl Const {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).module(db.upcast()) }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        db.const_data(self.id).name.clone()
    }

    pub fn value(self, db: &dyn HirDatabase) -> Option<ast::Expr> {
        self.source(db)?.value.body()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let data = db.const_data(self.id);
        let resolver = self.id.resolver(db.upcast());
        let ctx = hir_ty::TyLoweringContext::new(db, &resolver);
        let ty = ctx.lower_ty(&data.type_ref);
        Type::new_with_resolver_inner(db, &resolver, ty)
    }

    pub fn eval(self, db: &dyn HirDatabase) -> Result<ComputedExpr, ConstEvalError> {
        db.const_eval(self.id)
    }
}

impl HasVisibility for Const {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.const_visibility(self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Static {
    pub(crate) id: StaticId,
}

impl Static {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).module(db.upcast()) }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.static_data(self.id).name.clone()
    }

    pub fn is_mut(self, db: &dyn HirDatabase) -> bool {
        db.static_data(self.id).mutable
    }

    pub fn value(self, db: &dyn HirDatabase) -> Option<ast::Expr> {
        self.source(db)?.value.body()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let data = db.static_data(self.id);
        let resolver = self.id.resolver(db.upcast());
        let ctx = hir_ty::TyLoweringContext::new(db, &resolver);
        let ty = ctx.lower_ty(&data.type_ref);
        Type::new_with_resolver_inner(db, &resolver, ty)
    }
}

impl HasVisibility for Static {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.static_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trait {
    pub(crate) id: TraitId,
}

impl Trait {
    pub fn lang(db: &dyn HirDatabase, krate: Crate, name: &Name) -> Option<Trait> {
        db.lang_item(krate.into(), name.to_smol_str())
            .and_then(LangItemTarget::as_trait)
            .map(Into::into)
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.trait_data(self.id).name.clone()
    }

    pub fn items(self, db: &dyn HirDatabase) -> Vec<AssocItem> {
        db.trait_data(self.id).items.iter().map(|(_name, it)| (*it).into()).collect()
    }

    pub fn items_with_supertraits(self, db: &dyn HirDatabase) -> Vec<AssocItem> {
        let traits = all_super_traits(db.upcast(), self.into());
        traits.iter().flat_map(|tr| Trait::from(*tr).items(db)).collect()
    }

    pub fn is_auto(self, db: &dyn HirDatabase) -> bool {
        db.trait_data(self.id).is_auto
    }

    pub fn is_unsafe(&self, db: &dyn HirDatabase) -> bool {
        db.trait_data(self.id).is_unsafe
    }

    pub fn type_or_const_param_count(
        &self,
        db: &dyn HirDatabase,
        count_required_only: bool,
    ) -> usize {
        db.generic_params(GenericDefId::from(self.id))
            .type_or_consts
            .iter()
            .filter(|(_, ty)| match ty {
                TypeOrConstParamData::TypeParamData(ty)
                    if ty.provenance != TypeParamProvenance::TypeParamList =>
                {
                    false
                }
                _ => true,
            })
            .filter(|(_, ty)| !count_required_only || !ty.has_default())
            .count()
    }
}

impl HasVisibility for Trait {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.trait_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAlias {
    pub(crate) id: TypeAliasId,
}

impl TypeAlias {
    pub fn has_non_default_type_params(self, db: &dyn HirDatabase) -> bool {
        let subst = db.generic_defaults(self.id.into());
        subst.iter().any(|ty| match ty.skip_binders().data(Interner) {
            GenericArgData::Ty(x) => x.is_unknown(),
            _ => false,
        })
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).module(db.upcast()) }
    }

    pub fn type_ref(self, db: &dyn HirDatabase) -> Option<TypeRef> {
        db.type_alias_data(self.id).type_ref.as_deref().cloned()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id)
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.type_alias_data(self.id).name.clone()
    }
}

impl HasVisibility for TypeAlias {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let function_data = db.type_alias_data(self.id);
        let visibility = &function_data.visibility;
        visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuiltinType {
    pub(crate) inner: hir_def::builtin_type::BuiltinType,
}

impl BuiltinType {
    pub fn str() -> BuiltinType {
        BuiltinType { inner: hir_def::builtin_type::BuiltinType::Str }
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::new_for_crate(db.crate_graph().iter().next().unwrap(), TyBuilder::builtin(self.inner))
    }

    pub fn name(self) -> Name {
        self.inner.as_name()
    }

    pub fn is_int(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Int(_))
    }

    pub fn is_uint(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Uint(_))
    }

    pub fn is_float(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Float(_))
    }

    pub fn is_char(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Char)
    }

    pub fn is_bool(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Bool)
    }

    pub fn is_str(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroKind {
    /// `macro_rules!` or Macros 2.0 macro.
    Declarative,
    /// A built-in or custom derive.
    Derive,
    /// A built-in function-like macro.
    BuiltIn,
    /// A procedural attribute macro.
    Attr,
    /// A function-like procedural macro.
    ProcMacro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Macro {
    pub(crate) id: MacroId,
}

impl Macro {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.module(db.upcast()) }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        match self.id {
            MacroId::Macro2Id(id) => db.macro2_data(id).name.clone(),
            MacroId::MacroRulesId(id) => db.macro_rules_data(id).name.clone(),
            MacroId::ProcMacroId(id) => db.proc_macro_data(id).name.clone(),
        }
    }

    pub fn is_macro_export(self, db: &dyn HirDatabase) -> bool {
        matches!(self.id, MacroId::MacroRulesId(id) if db.macro_rules_data(id).macro_export)
    }

    pub fn kind(&self, db: &dyn HirDatabase) -> MacroKind {
        match self.id {
            MacroId::Macro2Id(it) => match it.lookup(db.upcast()).expander {
                MacroExpander::Declarative => MacroKind::Declarative,
                MacroExpander::BuiltIn(_) | MacroExpander::BuiltInEager(_) => MacroKind::BuiltIn,
                MacroExpander::BuiltInAttr(_) => MacroKind::Attr,
                MacroExpander::BuiltInDerive(_) => MacroKind::Derive,
            },
            MacroId::MacroRulesId(it) => match it.lookup(db.upcast()).expander {
                MacroExpander::Declarative => MacroKind::Declarative,
                MacroExpander::BuiltIn(_) | MacroExpander::BuiltInEager(_) => MacroKind::BuiltIn,
                MacroExpander::BuiltInAttr(_) => MacroKind::Attr,
                MacroExpander::BuiltInDerive(_) => MacroKind::Derive,
            },
            MacroId::ProcMacroId(it) => match it.lookup(db.upcast()).kind {
                ProcMacroKind::CustomDerive => MacroKind::Derive,
                ProcMacroKind::FuncLike => MacroKind::ProcMacro,
                ProcMacroKind::Attr => MacroKind::Attr,
            },
        }
    }

    pub fn is_fn_like(&self, db: &dyn HirDatabase) -> bool {
        match self.kind(db) {
            MacroKind::Declarative | MacroKind::BuiltIn | MacroKind::ProcMacro => true,
            MacroKind::Attr | MacroKind::Derive => false,
        }
    }

    pub fn is_builtin_derive(&self, db: &dyn HirDatabase) -> bool {
        match self.id {
            MacroId::Macro2Id(it) => {
                matches!(it.lookup(db.upcast()).expander, MacroExpander::BuiltInDerive(_))
            }
            MacroId::MacroRulesId(it) => {
                matches!(it.lookup(db.upcast()).expander, MacroExpander::BuiltInDerive(_))
            }
            MacroId::ProcMacroId(_) => false,
        }
    }

    pub fn is_attr(&self, db: &dyn HirDatabase) -> bool {
        matches!(self.kind(db), MacroKind::Attr)
    }

    pub fn is_derive(&self, db: &dyn HirDatabase) -> bool {
        matches!(self.kind(db), MacroKind::Derive)
    }
}

impl HasVisibility for Macro {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        match self.id {
            MacroId::Macro2Id(id) => {
                let data = db.macro2_data(id);
                let visibility = &data.visibility;
                visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
            }
            MacroId::MacroRulesId(_) => Visibility::Public,
            MacroId::ProcMacroId(_) => Visibility::Public,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ItemInNs {
    Types(ModuleDef),
    Values(ModuleDef),
    Macros(Macro),
}

impl From<Macro> for ItemInNs {
    fn from(it: Macro) -> Self {
        Self::Macros(it)
    }
}

impl From<ModuleDef> for ItemInNs {
    fn from(module_def: ModuleDef) -> Self {
        match module_def {
            ModuleDef::Static(_) | ModuleDef::Const(_) | ModuleDef::Function(_) => {
                ItemInNs::Values(module_def)
            }
            _ => ItemInNs::Types(module_def),
        }
    }
}

impl ItemInNs {
    pub fn as_module_def(self) -> Option<ModuleDef> {
        match self {
            ItemInNs::Types(id) | ItemInNs::Values(id) => Some(id),
            ItemInNs::Macros(_) => None,
        }
    }

    /// Returns the crate defining this item (or `None` if `self` is built-in).
    pub fn krate(&self, db: &dyn HirDatabase) -> Option<Crate> {
        match self {
            ItemInNs::Types(did) | ItemInNs::Values(did) => did.module(db).map(|m| m.krate()),
            ItemInNs::Macros(id) => Some(id.module(db).krate()),
        }
    }

    pub fn attrs(&self, db: &dyn HirDatabase) -> Option<AttrsWithOwner> {
        match self {
            ItemInNs::Types(it) | ItemInNs::Values(it) => it.attrs(db),
            ItemInNs::Macros(it) => Some(it.attrs(db)),
        }
    }
}

/// Invariant: `inner.as_assoc_item(db).is_some()`
/// We do not actively enforce this invariant.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AssocItem {
    Function(Function),
    Const(Const),
    TypeAlias(TypeAlias),
}
#[derive(Debug)]
pub enum AssocItemContainer {
    Trait(Trait),
    Impl(Impl),
}
pub trait AsAssocItem {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem>;
}

impl AsAssocItem for Function {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::Function, self.id)
    }
}
impl AsAssocItem for Const {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::Const, self.id)
    }
}
impl AsAssocItem for TypeAlias {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::TypeAlias, self.id)
    }
}
impl AsAssocItem for ModuleDef {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        match self {
            ModuleDef::Function(it) => it.as_assoc_item(db),
            ModuleDef::Const(it) => it.as_assoc_item(db),
            ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
            _ => None,
        }
    }
}
fn as_assoc_item<ID, DEF, CTOR, AST>(db: &dyn HirDatabase, ctor: CTOR, id: ID) -> Option<AssocItem>
where
    ID: Lookup<Data = AssocItemLoc<AST>>,
    DEF: From<ID>,
    CTOR: FnOnce(DEF) -> AssocItem,
    AST: ItemTreeNode,
{
    match id.lookup(db.upcast()).container {
        ItemContainerId::TraitId(_) | ItemContainerId::ImplId(_) => Some(ctor(DEF::from(id))),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
    }
}

impl AssocItem {
    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        match self {
            AssocItem::Function(it) => Some(it.name(db)),
            AssocItem::Const(it) => it.name(db),
            AssocItem::TypeAlias(it) => Some(it.name(db)),
        }
    }
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            AssocItem::Function(f) => f.module(db),
            AssocItem::Const(c) => c.module(db),
            AssocItem::TypeAlias(t) => t.module(db),
        }
    }
    pub fn container(self, db: &dyn HirDatabase) -> AssocItemContainer {
        let container = match self {
            AssocItem::Function(it) => it.id.lookup(db.upcast()).container,
            AssocItem::Const(it) => it.id.lookup(db.upcast()).container,
            AssocItem::TypeAlias(it) => it.id.lookup(db.upcast()).container,
        };
        match container {
            ItemContainerId::TraitId(id) => AssocItemContainer::Trait(id.into()),
            ItemContainerId::ImplId(id) => AssocItemContainer::Impl(id.into()),
            ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
                panic!("invalid AssocItem")
            }
        }
    }

    pub fn containing_trait(self, db: &dyn HirDatabase) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Trait(t) => Some(t),
            _ => None,
        }
    }

    pub fn containing_trait_impl(self, db: &dyn HirDatabase) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Impl(i) => i.trait_(db),
            _ => None,
        }
    }

    pub fn containing_trait_or_trait_impl(self, db: &dyn HirDatabase) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Trait(t) => Some(t),
            AssocItemContainer::Impl(i) => i.trait_(db),
        }
    }
}

impl HasVisibility for AssocItem {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        match self {
            AssocItem::Function(f) => f.visibility(db),
            AssocItem::Const(c) => c.visibility(db),
            AssocItem::TypeAlias(t) => t.visibility(db),
        }
    }
}

impl From<AssocItem> for ModuleDef {
    fn from(assoc: AssocItem) -> Self {
        match assoc {
            AssocItem::Function(it) => ModuleDef::Function(it),
            AssocItem::Const(it) => ModuleDef::Const(it),
            AssocItem::TypeAlias(it) => ModuleDef::TypeAlias(it),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum GenericDef {
    Function(Function),
    Adt(Adt),
    Trait(Trait),
    TypeAlias(TypeAlias),
    Impl(Impl),
    // enum variants cannot have generics themselves, but their parent enums
    // can, and this makes some code easier to write
    Variant(Variant),
    // consts can have type parameters from their parents (i.e. associated consts of traits)
    Const(Const),
}
impl_from!(
    Function,
    Adt(Struct, Enum, Union),
    Trait,
    TypeAlias,
    Impl,
    Variant,
    Const
    for GenericDef
);

impl GenericDef {
    pub fn params(self, db: &dyn HirDatabase) -> Vec<GenericParam> {
        let generics = db.generic_params(self.into());
        let ty_params = generics.type_or_consts.iter().map(|(local_id, _)| {
            let toc = TypeOrConstParam { id: TypeOrConstParamId { parent: self.into(), local_id } };
            match toc.split(db) {
                Either::Left(x) => GenericParam::ConstParam(x),
                Either::Right(x) => GenericParam::TypeParam(x),
            }
        });
        let lt_params = generics
            .lifetimes
            .iter()
            .map(|(local_id, _)| LifetimeParam {
                id: LifetimeParamId { parent: self.into(), local_id },
            })
            .map(GenericParam::LifetimeParam);
        lt_params.chain(ty_params).collect()
    }

    pub fn type_params(self, db: &dyn HirDatabase) -> Vec<TypeOrConstParam> {
        let generics = db.generic_params(self.into());
        generics
            .type_or_consts
            .iter()
            .map(|(local_id, _)| TypeOrConstParam {
                id: TypeOrConstParamId { parent: self.into(), local_id },
            })
            .collect()
    }
}

/// A single local definition.
///
/// If the definition of this is part of a "MultiLocal", that is a local that has multiple declarations due to or-patterns
/// then this only references a single one of those.
/// To retrieve the other locals you should use [`Local::associated_locals`]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Local {
    pub(crate) parent: DefWithBodyId,
    pub(crate) pat_id: PatId,
}

impl Local {
    pub fn is_param(self, db: &dyn HirDatabase) -> bool {
        let src = self.source(db);
        match src.value {
            Either::Left(pat) => pat
                .syntax()
                .ancestors()
                .map(|it| it.kind())
                .take_while(|&kind| ast::Pat::can_cast(kind) || ast::Param::can_cast(kind))
                .any(ast::Param::can_cast),
            Either::Right(_) => true,
        }
    }

    pub fn as_self_param(self, db: &dyn HirDatabase) -> Option<SelfParam> {
        match self.parent {
            DefWithBodyId::FunctionId(func) if self.is_self(db) => Some(SelfParam { func }),
            _ => None,
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let body = db.body(self.parent);
        match &body[self.pat_id] {
            Pat::Bind { name, .. } => name.clone(),
            _ => {
                stdx::never!("hir::Local is missing a name!");
                Name::missing()
            }
        }
    }

    pub fn is_self(self, db: &dyn HirDatabase) -> bool {
        self.name(db) == name![self]
    }

    pub fn is_mut(self, db: &dyn HirDatabase) -> bool {
        let body = db.body(self.parent);
        matches!(&body[self.pat_id], Pat::Bind { mode: BindingAnnotation::Mutable, .. })
    }

    pub fn is_ref(self, db: &dyn HirDatabase) -> bool {
        let body = db.body(self.parent);
        matches!(
            &body[self.pat_id],
            Pat::Bind { mode: BindingAnnotation::Ref | BindingAnnotation::RefMut, .. }
        )
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> DefWithBody {
        self.parent.into()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.parent(db).module(db)
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let def = self.parent;
        let infer = db.infer(def);
        let ty = infer[self.pat_id].clone();
        Type::new(db, def, ty)
    }

    pub fn associated_locals(self, db: &dyn HirDatabase) -> Box<[Local]> {
        let body = db.body(self.parent);
        body.ident_patterns_for(&self.pat_id)
            .iter()
            .map(|&pat_id| Local { parent: self.parent, pat_id })
            .collect()
    }

    /// If this local is part of a multi-local, retrieve the representative local.
    /// That is the local that references are being resolved to.
    pub fn representative(self, db: &dyn HirDatabase) -> Local {
        let body = db.body(self.parent);
        Local { pat_id: body.pattern_representative(self.pat_id), ..self }
    }

    pub fn source(self, db: &dyn HirDatabase) -> InFile<Either<ast::IdentPat, ast::SelfParam>> {
        let (_body, source_map) = db.body_with_source_map(self.parent);
        let src = source_map.pat_syntax(self.pat_id).unwrap(); // Hmm...
        let root = src.file_syntax(db.upcast());
        src.map(|ast| match ast {
            // Suspicious unwrap
            Either::Left(it) => Either::Left(it.cast().unwrap().to_node(&root)),
            Either::Right(it) => Either::Right(it.to_node(&root)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeriveHelper {
    pub(crate) derive: MacroId,
    pub(crate) idx: usize,
}

impl DeriveHelper {
    pub fn derive(&self) -> Macro {
        Macro { id: self.derive.into() }
    }

    pub fn name(&self, db: &dyn HirDatabase) -> Name {
        match self.derive {
            MacroId::Macro2Id(_) => None,
            MacroId::MacroRulesId(_) => None,
            MacroId::ProcMacroId(proc_macro) => db
                .proc_macro_data(proc_macro)
                .helpers
                .as_ref()
                .and_then(|it| it.get(self.idx))
                .cloned(),
        }
        .unwrap_or_else(|| Name::missing())
    }
}

// FIXME: Wrong name? This is could also be a registered attribute
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BuiltinAttr {
    krate: Option<CrateId>,
    idx: usize,
}

impl BuiltinAttr {
    // FIXME: consider crates\hir_def\src\nameres\attr_resolution.rs?
    pub(crate) fn by_name(db: &dyn HirDatabase, krate: Crate, name: &str) -> Option<Self> {
        if let builtin @ Some(_) = Self::builtin(name) {
            return builtin;
        }
        let idx = db.crate_def_map(krate.id).registered_attrs().iter().position(|it| it == name)?;
        Some(BuiltinAttr { krate: Some(krate.id), idx })
    }

    fn builtin(name: &str) -> Option<Self> {
        hir_def::builtin_attr::INERT_ATTRIBUTES
            .iter()
            .position(|tool| tool.name == name)
            .map(|idx| BuiltinAttr { krate: None, idx })
    }

    pub fn name(&self, db: &dyn HirDatabase) -> SmolStr {
        // FIXME: Return a `Name` here
        match self.krate {
            Some(krate) => db.crate_def_map(krate).registered_attrs()[self.idx].clone(),
            None => SmolStr::new(hir_def::builtin_attr::INERT_ATTRIBUTES[self.idx].name),
        }
    }

    pub fn template(&self, _: &dyn HirDatabase) -> Option<AttributeTemplate> {
        match self.krate {
            Some(_) => None,
            None => Some(hir_def::builtin_attr::INERT_ATTRIBUTES[self.idx].template),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolModule {
    krate: Option<CrateId>,
    idx: usize,
}

impl ToolModule {
    // FIXME: consider crates\hir_def\src\nameres\attr_resolution.rs?
    pub(crate) fn by_name(db: &dyn HirDatabase, krate: Crate, name: &str) -> Option<Self> {
        if let builtin @ Some(_) = Self::builtin(name) {
            return builtin;
        }
        let idx = db.crate_def_map(krate.id).registered_tools().iter().position(|it| it == name)?;
        Some(ToolModule { krate: Some(krate.id), idx })
    }

    fn builtin(name: &str) -> Option<Self> {
        hir_def::builtin_attr::TOOL_MODULES
            .iter()
            .position(|&tool| tool == name)
            .map(|idx| ToolModule { krate: None, idx })
    }

    pub fn name(&self, db: &dyn HirDatabase) -> SmolStr {
        // FIXME: Return a `Name` here
        match self.krate {
            Some(krate) => db.crate_def_map(krate).registered_tools()[self.idx].clone(),
            None => SmolStr::new(hir_def::builtin_attr::TOOL_MODULES[self.idx]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Label {
    pub(crate) parent: DefWithBodyId,
    pub(crate) label_id: LabelId,
}

impl Label {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.parent(db).module(db)
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> DefWithBody {
        self.parent.into()
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let body = db.body(self.parent);
        body[self.label_id].name.clone()
    }

    pub fn source(self, db: &dyn HirDatabase) -> InFile<ast::Label> {
        let (_body, source_map) = db.body_with_source_map(self.parent);
        let src = source_map.label_syntax(self.label_id);
        let root = src.file_syntax(db.upcast());
        src.map(|ast| ast.to_node(&root))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenericParam {
    TypeParam(TypeParam),
    ConstParam(ConstParam),
    LifetimeParam(LifetimeParam),
}
impl_from!(TypeParam, ConstParam, LifetimeParam for GenericParam);

impl GenericParam {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            GenericParam::TypeParam(it) => it.module(db),
            GenericParam::ConstParam(it) => it.module(db),
            GenericParam::LifetimeParam(it) => it.module(db),
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        match self {
            GenericParam::TypeParam(it) => it.name(db),
            GenericParam::ConstParam(it) => it.name(db),
            GenericParam::LifetimeParam(it) => it.name(db),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeParam {
    pub(crate) id: TypeParamId,
}

impl TypeParam {
    pub fn merge(self) -> TypeOrConstParam {
        TypeOrConstParam { id: self.id.into() }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        self.merge().name(db)
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent().module(db.upcast()).into()
    }

    /// Is this type parameter implicitly introduced (eg. `Self` in a trait or an `impl Trait`
    /// argument)?
    pub fn is_implicit(self, db: &dyn HirDatabase) -> bool {
        let params = db.generic_params(self.id.parent());
        let data = &params.type_or_consts[self.id.local_id()];
        match data.type_param().unwrap().provenance {
            hir_def::generics::TypeParamProvenance::TypeParamList => false,
            hir_def::generics::TypeParamProvenance::TraitSelf
            | hir_def::generics::TypeParamProvenance::ArgumentImplTrait => true,
        }
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let resolver = self.id.parent().resolver(db.upcast());
        let ty =
            TyKind::Placeholder(hir_ty::to_placeholder_idx(db, self.id.into())).intern(Interner);
        Type::new_with_resolver_inner(db, &resolver, ty)
    }

    /// FIXME: this only lists trait bounds from the item defining the type
    /// parameter, not additional bounds that might be added e.g. by a method if
    /// the parameter comes from an impl!
    pub fn trait_bounds(self, db: &dyn HirDatabase) -> Vec<Trait> {
        db.generic_predicates_for_param(self.id.parent(), self.id.into(), None)
            .iter()
            .filter_map(|pred| match &pred.skip_binders().skip_binders() {
                hir_ty::WhereClause::Implemented(trait_ref) => {
                    Some(Trait::from(trait_ref.hir_trait_id()))
                }
                _ => None,
            })
            .collect()
    }

    pub fn default(self, db: &dyn HirDatabase) -> Option<Type> {
        let params = db.generic_defaults(self.id.parent());
        let local_idx = hir_ty::param_idx(db, self.id.into())?;
        let resolver = self.id.parent().resolver(db.upcast());
        let ty = params.get(local_idx)?.clone();
        let subst = TyBuilder::placeholder_subst(db, self.id.parent());
        let ty = ty.substitute(Interner, &subst_prefix(&subst, local_idx));
        match ty.data(Interner) {
            GenericArgData::Ty(x) => Some(Type::new_with_resolver_inner(db, &resolver, x.clone())),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LifetimeParam {
    pub(crate) id: LifetimeParamId,
}

impl LifetimeParam {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let params = db.generic_params(self.id.parent);
        params.lifetimes[self.id.local_id].name.clone()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent.module(db.upcast()).into()
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> GenericDef {
        self.id.parent.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConstParam {
    pub(crate) id: ConstParamId,
}

impl ConstParam {
    pub fn merge(self) -> TypeOrConstParam {
        TypeOrConstParam { id: self.id.into() }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let params = db.generic_params(self.id.parent());
        match params.type_or_consts[self.id.local_id()].name() {
            Some(x) => x.clone(),
            None => {
                never!();
                Name::missing()
            }
        }
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent().module(db.upcast()).into()
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> GenericDef {
        self.id.parent().into()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::new(db, self.id.parent(), db.const_param_ty(self.id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeOrConstParam {
    pub(crate) id: TypeOrConstParamId,
}

impl TypeOrConstParam {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let params = db.generic_params(self.id.parent);
        match params.type_or_consts[self.id.local_id].name() {
            Some(n) => n.clone(),
            _ => Name::missing(),
        }
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent.module(db.upcast()).into()
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> GenericDef {
        self.id.parent.into()
    }

    pub fn split(self, db: &dyn HirDatabase) -> Either<ConstParam, TypeParam> {
        let params = db.generic_params(self.id.parent);
        match &params.type_or_consts[self.id.local_id] {
            hir_def::generics::TypeOrConstParamData::TypeParamData(_) => {
                Either::Right(TypeParam { id: TypeParamId::from_unchecked(self.id) })
            }
            hir_def::generics::TypeOrConstParamData::ConstParamData(_) => {
                Either::Left(ConstParam { id: ConstParamId::from_unchecked(self.id) })
            }
        }
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        match self.split(db) {
            Either::Left(x) => x.ty(db),
            Either::Right(x) => x.ty(db),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Impl {
    pub(crate) id: ImplId,
}

impl Impl {
    pub fn all_in_crate(db: &dyn HirDatabase, krate: Crate) -> Vec<Impl> {
        let inherent = db.inherent_impls_in_crate(krate.id);
        let trait_ = db.trait_impls_in_crate(krate.id);

        inherent.all_impls().chain(trait_.all_impls()).map(Self::from).collect()
    }

    pub fn all_for_type(db: &dyn HirDatabase, Type { ty, env }: Type) -> Vec<Impl> {
        let def_crates = match method_resolution::def_crates(db, &ty, env.krate) {
            Some(def_crates) => def_crates,
            None => return Vec::new(),
        };

        let filter = |impl_def: &Impl| {
            let self_ty = impl_def.self_ty(db);
            let rref = self_ty.remove_ref();
            ty.equals_ctor(rref.as_ref().map_or(&self_ty.ty, |it| &it.ty))
        };

        let fp = TyFingerprint::for_inherent_impl(&ty);
        let fp = match fp {
            Some(fp) => fp,
            None => return Vec::new(),
        };

        let mut all = Vec::new();
        def_crates.iter().for_each(|&id| {
            all.extend(
                db.inherent_impls_in_crate(id)
                    .for_self_ty(&ty)
                    .iter()
                    .cloned()
                    .map(Self::from)
                    .filter(filter),
            )
        });
        for id in def_crates
            .iter()
            .flat_map(|&id| Crate { id }.transitive_reverse_dependencies(db))
            .map(|Crate { id }| id)
            .chain(def_crates.iter().copied())
            .unique()
        {
            all.extend(
                db.trait_impls_in_crate(id)
                    .for_self_ty_without_blanket_impls(fp)
                    .map(Self::from)
                    .filter(filter),
            );
        }
        all
    }

    pub fn all_for_trait(db: &dyn HirDatabase, trait_: Trait) -> Vec<Impl> {
        let krate = trait_.module(db).krate();
        let mut all = Vec::new();
        for Crate { id } in krate.transitive_reverse_dependencies(db).into_iter() {
            let impls = db.trait_impls_in_crate(id);
            all.extend(impls.for_trait(trait_.id).map(Self::from))
        }
        all
    }

    // FIXME: the return type is wrong. This should be a hir version of
    // `TraitRef` (to account for parameters and qualifiers)
    pub fn trait_(self, db: &dyn HirDatabase) -> Option<Trait> {
        let trait_ref = db.impl_trait(self.id)?.skip_binders().clone();
        let id = hir_ty::from_chalk_trait_id(trait_ref.trait_id);
        Some(Trait { id })
    }

    pub fn self_ty(self, db: &dyn HirDatabase) -> Type {
        let resolver = self.id.resolver(db.upcast());
        let substs = TyBuilder::placeholder_subst(db, self.id);
        let ty = db.impl_self_ty(self.id).substitute(Interner, &substs);
        Type::new_with_resolver_inner(db, &resolver, ty)
    }

    pub fn items(self, db: &dyn HirDatabase) -> Vec<AssocItem> {
        db.impl_data(self.id).items.iter().map(|it| (*it).into()).collect()
    }

    pub fn is_negative(self, db: &dyn HirDatabase) -> bool {
        db.impl_data(self.id).is_negative
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.lookup(db.upcast()).container.into()
    }

    pub fn is_builtin_derive(self, db: &dyn HirDatabase) -> Option<InFile<ast::Attr>> {
        let src = self.source(db)?;
        src.file_id.is_builtin_derive(db.upcast())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Type {
    env: Arc<TraitEnvironment>,
    ty: Ty,
}

impl Type {
    pub(crate) fn new_with_resolver(db: &dyn HirDatabase, resolver: &Resolver, ty: Ty) -> Type {
        Type::new_with_resolver_inner(db, resolver, ty)
    }

    pub(crate) fn new_with_resolver_inner(
        db: &dyn HirDatabase,
        resolver: &Resolver,
        ty: Ty,
    ) -> Type {
        let environment = resolver.generic_def().map_or_else(
            || Arc::new(TraitEnvironment::empty(resolver.krate())),
            |d| db.trait_environment(d),
        );
        Type { env: environment, ty }
    }

    pub(crate) fn new_for_crate(krate: CrateId, ty: Ty) -> Type {
        Type { env: Arc::new(TraitEnvironment::empty(krate)), ty }
    }

    pub fn reference(inner: &Type, m: Mutability) -> Type {
        inner.derived(
            TyKind::Ref(
                if m.is_mut() { hir_ty::Mutability::Mut } else { hir_ty::Mutability::Not },
                hir_ty::static_lifetime(),
                inner.ty.clone(),
            )
            .intern(Interner),
        )
    }

    fn new(db: &dyn HirDatabase, lexical_env: impl HasResolver, ty: Ty) -> Type {
        let resolver = lexical_env.resolver(db.upcast());
        let environment = resolver.generic_def().map_or_else(
            || Arc::new(TraitEnvironment::empty(resolver.krate())),
            |d| db.trait_environment(d),
        );
        Type { env: environment, ty }
    }

    fn from_def(db: &dyn HirDatabase, def: impl HasResolver + Into<TyDefId>) -> Type {
        let ty = TyBuilder::def_ty(db, def.into()).fill_with_unknown().build();
        Type::new(db, def, ty)
    }

    pub fn new_slice(ty: Type) -> Type {
        Type { env: ty.env, ty: TyBuilder::slice(ty.ty) }
    }

    pub fn is_unit(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Tuple(0, ..))
    }

    pub fn is_bool(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Scalar(Scalar::Bool))
    }

    pub fn is_never(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Never)
    }

    pub fn is_mutable_reference(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Ref(hir_ty::Mutability::Mut, ..))
    }

    pub fn is_reference(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Ref(..))
    }

    pub fn as_reference(&self) -> Option<(Type, Mutability)> {
        let (ty, _lt, m) = self.ty.as_reference()?;
        let m = Mutability::from_mutable(matches!(m, hir_ty::Mutability::Mut));
        Some((self.derived(ty.clone()), m))
    }

    pub fn is_slice(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Slice(..))
    }

    pub fn is_usize(&self) -> bool {
        matches!(self.ty.kind(Interner), TyKind::Scalar(Scalar::Uint(UintTy::Usize)))
    }

    pub fn remove_ref(&self) -> Option<Type> {
        match &self.ty.kind(Interner) {
            TyKind::Ref(.., ty) => Some(self.derived(ty.clone())),
            _ => None,
        }
    }

    pub fn strip_references(&self) -> Type {
        self.derived(self.ty.strip_references().clone())
    }

    pub fn strip_reference(&self) -> Type {
        self.derived(self.ty.strip_reference().clone())
    }

    pub fn is_unknown(&self) -> bool {
        self.ty.is_unknown()
    }

    /// Checks that particular type `ty` implements `std::future::IntoFuture` or
    /// `std::future::Future`.
    /// This function is used in `.await` syntax completion.
    pub fn impls_into_future(&self, db: &dyn HirDatabase) -> bool {
        let trait_ = db
            .lang_item(self.env.krate, SmolStr::new_inline("into_future"))
            .and_then(|it| {
                let into_future_fn = it.as_function()?;
                let assoc_item = as_assoc_item(db, AssocItem::Function, into_future_fn)?;
                let into_future_trait = assoc_item.containing_trait_or_trait_impl(db)?;
                Some(into_future_trait.id)
            })
            .or_else(|| {
                let future_trait =
                    db.lang_item(self.env.krate, SmolStr::new_inline("future_trait"))?;
                future_trait.as_trait()
            });

        let trait_ = match trait_ {
            Some(it) => it,
            None => return false,
        };

        let canonical_ty =
            Canonical { value: self.ty.clone(), binders: CanonicalVarKinds::empty(Interner) };
        method_resolution::implements_trait(&canonical_ty, db, self.env.clone(), trait_)
    }

    /// Checks that particular type `ty` implements `std::ops::FnOnce`.
    ///
    /// This function can be used to check if a particular type is callable, since FnOnce is a
    /// supertrait of Fn and FnMut, so all callable types implements at least FnOnce.
    pub fn impls_fnonce(&self, db: &dyn HirDatabase) -> bool {
        let fnonce_trait = match FnTrait::FnOnce.get_id(db, self.env.krate) {
            Some(it) => it,
            None => return false,
        };

        let canonical_ty =
            Canonical { value: self.ty.clone(), binders: CanonicalVarKinds::empty(Interner) };
        method_resolution::implements_trait_unique(
            &canonical_ty,
            db,
            self.env.clone(),
            fnonce_trait,
        )
    }

    pub fn impls_trait(&self, db: &dyn HirDatabase, trait_: Trait, args: &[Type]) -> bool {
        let mut it = args.iter().map(|t| t.ty.clone());
        let trait_ref = TyBuilder::trait_ref(db, trait_.id)
            .push(self.ty.clone())
            .fill(|x| {
                let r = it.next().unwrap();
                match x {
                    ParamKind::Type => GenericArgData::Ty(r).intern(Interner),
                    ParamKind::Const(ty) => {
                        // FIXME: this code is not covered in tests.
                        unknown_const_as_generic(ty.clone())
                    }
                }
            })
            .build();

        let goal = Canonical {
            value: hir_ty::InEnvironment::new(&self.env.env, trait_ref.cast(Interner)),
            binders: CanonicalVarKinds::empty(Interner),
        };

        db.trait_solve(self.env.krate, goal).is_some()
    }

    pub fn normalize_trait_assoc_type(
        &self,
        db: &dyn HirDatabase,
        args: &[Type],
        alias: TypeAlias,
    ) -> Option<Type> {
        let mut args = args.iter();
        let projection = TyBuilder::assoc_type_projection(db, alias.id)
            .push(self.ty.clone())
            .fill(|x| {
                // FIXME: this code is not covered in tests.
                match x {
                    ParamKind::Type => {
                        GenericArgData::Ty(args.next().unwrap().ty.clone()).intern(Interner)
                    }
                    ParamKind::Const(ty) => unknown_const_as_generic(ty.clone()),
                }
            })
            .build();

        let ty = db.normalize_projection(projection, self.env.clone());
        if ty.is_unknown() {
            None
        } else {
            Some(self.derived(ty))
        }
    }

    pub fn is_copy(&self, db: &dyn HirDatabase) -> bool {
        let lang_item = db.lang_item(self.env.krate, SmolStr::new_inline("copy"));
        let copy_trait = match lang_item {
            Some(LangItemTarget::TraitId(it)) => it,
            _ => return false,
        };
        self.impls_trait(db, copy_trait.into(), &[])
    }

    pub fn as_callable(&self, db: &dyn HirDatabase) -> Option<Callable> {
        let callee = match self.ty.kind(Interner) {
            TyKind::Closure(id, _) => Callee::Closure(*id),
            TyKind::Function(_) => Callee::FnPtr,
            _ => Callee::Def(self.ty.callable_def(db)?),
        };

        let sig = self.ty.callable_sig(db)?;
        Some(Callable { ty: self.clone(), sig, callee, is_bound_method: false })
    }

    pub fn is_closure(&self) -> bool {
        matches!(&self.ty.kind(Interner), TyKind::Closure { .. })
    }

    pub fn is_fn(&self) -> bool {
        matches!(&self.ty.kind(Interner), TyKind::FnDef(..) | TyKind::Function { .. })
    }

    pub fn is_array(&self) -> bool {
        matches!(&self.ty.kind(Interner), TyKind::Array(..))
    }

    pub fn is_packed(&self, db: &dyn HirDatabase) -> bool {
        let adt_id = match *self.ty.kind(Interner) {
            TyKind::Adt(hir_ty::AdtId(adt_id), ..) => adt_id,
            _ => return false,
        };

        let adt = adt_id.into();
        match adt {
            Adt::Struct(s) => matches!(s.repr(db), Some(ReprData { packed: true, .. })),
            _ => false,
        }
    }

    pub fn is_raw_ptr(&self) -> bool {
        matches!(&self.ty.kind(Interner), TyKind::Raw(..))
    }

    pub fn contains_unknown(&self) -> bool {
        return go(&self.ty);

        fn go(ty: &Ty) -> bool {
            match ty.kind(Interner) {
                TyKind::Error => true,

                TyKind::Adt(_, substs)
                | TyKind::AssociatedType(_, substs)
                | TyKind::Tuple(_, substs)
                | TyKind::OpaqueType(_, substs)
                | TyKind::FnDef(_, substs)
                | TyKind::Closure(_, substs) => {
                    substs.iter(Interner).filter_map(|a| a.ty(Interner)).any(go)
                }

                TyKind::Array(_ty, len) if len.is_unknown() => true,
                TyKind::Array(ty, _)
                | TyKind::Slice(ty)
                | TyKind::Raw(_, ty)
                | TyKind::Ref(_, _, ty) => go(ty),

                TyKind::Scalar(_)
                | TyKind::Str
                | TyKind::Never
                | TyKind::Placeholder(_)
                | TyKind::BoundVar(_)
                | TyKind::InferenceVar(_, _)
                | TyKind::Dyn(_)
                | TyKind::Function(_)
                | TyKind::Alias(_)
                | TyKind::Foreign(_)
                | TyKind::Generator(..)
                | TyKind::GeneratorWitness(..) => false,
            }
        }
    }

    pub fn fields(&self, db: &dyn HirDatabase) -> Vec<(Field, Type)> {
        let (variant_id, substs) = match self.ty.kind(Interner) {
            TyKind::Adt(hir_ty::AdtId(AdtId::StructId(s)), substs) => ((*s).into(), substs),
            TyKind::Adt(hir_ty::AdtId(AdtId::UnionId(u)), substs) => ((*u).into(), substs),
            _ => return Vec::new(),
        };

        db.field_types(variant_id)
            .iter()
            .map(|(local_id, ty)| {
                let def = Field { parent: variant_id.into(), id: local_id };
                let ty = ty.clone().substitute(Interner, substs);
                (def, self.derived(ty))
            })
            .collect()
    }

    pub fn tuple_fields(&self, _db: &dyn HirDatabase) -> Vec<Type> {
        if let TyKind::Tuple(_, substs) = &self.ty.kind(Interner) {
            substs
                .iter(Interner)
                .map(|ty| self.derived(ty.assert_ty_ref(Interner).clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn autoderef<'a>(&'a self, db: &'a dyn HirDatabase) -> impl Iterator<Item = Type> + 'a {
        self.autoderef_(db).map(move |ty| self.derived(ty))
    }

    fn autoderef_<'a>(&'a self, db: &'a dyn HirDatabase) -> impl Iterator<Item = Ty> + 'a {
        // There should be no inference vars in types passed here
        let canonical = hir_ty::replace_errors_with_variables(&self.ty);
        let environment = self.env.clone();
        autoderef(db, environment, canonical).map(|canonical| canonical.value)
    }

    // This would be nicer if it just returned an iterator, but that runs into
    // lifetime problems, because we need to borrow temp `CrateImplDefs`.
    pub fn iterate_assoc_items<T>(
        &self,
        db: &dyn HirDatabase,
        krate: Crate,
        mut callback: impl FnMut(AssocItem) -> Option<T>,
    ) -> Option<T> {
        let mut slot = None;
        self.iterate_assoc_items_dyn(db, krate, &mut |assoc_item_id| {
            slot = callback(assoc_item_id.into());
            slot.is_some()
        });
        slot
    }

    fn iterate_assoc_items_dyn(
        &self,
        db: &dyn HirDatabase,
        krate: Crate,
        callback: &mut dyn FnMut(AssocItemId) -> bool,
    ) {
        let def_crates = match method_resolution::def_crates(db, &self.ty, krate.id) {
            Some(it) => it,
            None => return,
        };
        for krate in def_crates {
            let impls = db.inherent_impls_in_crate(krate);

            for impl_def in impls.for_self_ty(&self.ty) {
                for &item in db.impl_data(*impl_def).items.iter() {
                    if callback(item) {
                        return;
                    }
                }
            }
        }
    }

    pub fn type_arguments(&self) -> impl Iterator<Item = Type> + '_ {
        self.ty
            .strip_references()
            .as_adt()
            .into_iter()
            .flat_map(|(_, substs)| substs.iter(Interner))
            .filter_map(|arg| arg.ty(Interner).cloned())
            .map(move |ty| self.derived(ty))
    }

    pub fn iterate_method_candidates<T>(
        &self,
        db: &dyn HirDatabase,
        scope: &SemanticsScope<'_>,
        // FIXME this can be retrieved from `scope`, except autoimport uses this
        // to specify a different set, so the method needs to be split
        traits_in_scope: &FxHashSet<TraitId>,
        with_local_impls: Option<Module>,
        name: Option<&Name>,
        mut callback: impl FnMut(Function) -> Option<T>,
    ) -> Option<T> {
        let _p = profile::span("iterate_method_candidates");
        let mut slot = None;

        self.iterate_method_candidates_dyn(
            db,
            scope,
            traits_in_scope,
            with_local_impls,
            name,
            &mut |assoc_item_id| {
                if let AssocItemId::FunctionId(func) = assoc_item_id {
                    if let Some(res) = callback(func.into()) {
                        slot = Some(res);
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            },
        );
        slot
    }

    fn iterate_method_candidates_dyn(
        &self,
        db: &dyn HirDatabase,
        scope: &SemanticsScope<'_>,
        traits_in_scope: &FxHashSet<TraitId>,
        with_local_impls: Option<Module>,
        name: Option<&Name>,
        callback: &mut dyn FnMut(AssocItemId) -> ControlFlow<()>,
    ) {
        // There should be no inference vars in types passed here
        let canonical = hir_ty::replace_errors_with_variables(&self.ty);

        let krate = scope.krate();
        let environment = scope.resolver().generic_def().map_or_else(
            || Arc::new(TraitEnvironment::empty(krate.id)),
            |d| db.trait_environment(d),
        );

        method_resolution::iterate_method_candidates_dyn(
            &canonical,
            db,
            environment,
            traits_in_scope,
            with_local_impls.and_then(|b| b.id.containing_block()).into(),
            name,
            method_resolution::LookupMode::MethodCall,
            &mut |_adj, id| callback(id),
        );
    }

    pub fn iterate_path_candidates<T>(
        &self,
        db: &dyn HirDatabase,
        scope: &SemanticsScope<'_>,
        traits_in_scope: &FxHashSet<TraitId>,
        with_local_impls: Option<Module>,
        name: Option<&Name>,
        mut callback: impl FnMut(AssocItem) -> Option<T>,
    ) -> Option<T> {
        let _p = profile::span("iterate_path_candidates");
        let mut slot = None;
        self.iterate_path_candidates_dyn(
            db,
            scope,
            traits_in_scope,
            with_local_impls,
            name,
            &mut |assoc_item_id| {
                if let Some(res) = callback(assoc_item_id.into()) {
                    slot = Some(res);
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            },
        );
        slot
    }

    fn iterate_path_candidates_dyn(
        &self,
        db: &dyn HirDatabase,
        scope: &SemanticsScope<'_>,
        traits_in_scope: &FxHashSet<TraitId>,
        with_local_impls: Option<Module>,
        name: Option<&Name>,
        callback: &mut dyn FnMut(AssocItemId) -> ControlFlow<()>,
    ) {
        let canonical = hir_ty::replace_errors_with_variables(&self.ty);

        let krate = scope.krate();
        let environment = scope.resolver().generic_def().map_or_else(
            || Arc::new(TraitEnvironment::empty(krate.id)),
            |d| db.trait_environment(d),
        );

        method_resolution::iterate_path_candidates(
            &canonical,
            db,
            environment,
            traits_in_scope,
            with_local_impls.and_then(|b| b.id.containing_block()).into(),
            name,
            &mut |id| callback(id),
        );
    }

    pub fn as_adt(&self) -> Option<Adt> {
        let (adt, _subst) = self.ty.as_adt()?;
        Some(adt.into())
    }

    pub fn as_builtin(&self) -> Option<BuiltinType> {
        self.ty.as_builtin().map(|inner| BuiltinType { inner })
    }

    pub fn as_dyn_trait(&self) -> Option<Trait> {
        self.ty.dyn_trait().map(Into::into)
    }

    /// If a type can be represented as `dyn Trait`, returns all traits accessible via this type,
    /// or an empty iterator otherwise.
    pub fn applicable_inherent_traits<'a>(
        &'a self,
        db: &'a dyn HirDatabase,
    ) -> impl Iterator<Item = Trait> + 'a {
        let _p = profile::span("applicable_inherent_traits");
        self.autoderef_(db)
            .filter_map(|ty| ty.dyn_trait())
            .flat_map(move |dyn_trait_id| hir_ty::all_super_traits(db.upcast(), dyn_trait_id))
            .map(Trait::from)
    }

    pub fn env_traits<'a>(&'a self, db: &'a dyn HirDatabase) -> impl Iterator<Item = Trait> + 'a {
        let _p = profile::span("env_traits");
        self.autoderef_(db)
            .filter(|ty| matches!(ty.kind(Interner), TyKind::Placeholder(_)))
            .flat_map(|ty| {
                self.env
                    .traits_in_scope_from_clauses(ty)
                    .flat_map(|t| hir_ty::all_super_traits(db.upcast(), t))
            })
            .map(Trait::from)
    }

    pub fn as_impl_traits(&self, db: &dyn HirDatabase) -> Option<impl Iterator<Item = Trait>> {
        self.ty.impl_trait_bounds(db).map(|it| {
            it.into_iter().filter_map(|pred| match pred.skip_binders() {
                hir_ty::WhereClause::Implemented(trait_ref) => {
                    Some(Trait::from(trait_ref.hir_trait_id()))
                }
                _ => None,
            })
        })
    }

    pub fn as_associated_type_parent_trait(&self, db: &dyn HirDatabase) -> Option<Trait> {
        self.ty.associated_type_parent_trait(db).map(Into::into)
    }

    fn derived(&self, ty: Ty) -> Type {
        Type { env: self.env.clone(), ty }
    }

    pub fn walk(&self, db: &dyn HirDatabase, mut cb: impl FnMut(Type)) {
        // TypeWalk::walk for a Ty at first visits parameters and only after that the Ty itself.
        // We need a different order here.

        fn walk_substs(
            db: &dyn HirDatabase,
            type_: &Type,
            substs: &Substitution,
            cb: &mut impl FnMut(Type),
        ) {
            for ty in substs.iter(Interner).filter_map(|a| a.ty(Interner)) {
                walk_type(db, &type_.derived(ty.clone()), cb);
            }
        }

        fn walk_bounds(
            db: &dyn HirDatabase,
            type_: &Type,
            bounds: &[QuantifiedWhereClause],
            cb: &mut impl FnMut(Type),
        ) {
            for pred in bounds {
                if let WhereClause::Implemented(trait_ref) = pred.skip_binders() {
                    cb(type_.clone());
                    // skip the self type. it's likely the type we just got the bounds from
                    for ty in
                        trait_ref.substitution.iter(Interner).skip(1).filter_map(|a| a.ty(Interner))
                    {
                        walk_type(db, &type_.derived(ty.clone()), cb);
                    }
                }
            }
        }

        fn walk_type(db: &dyn HirDatabase, type_: &Type, cb: &mut impl FnMut(Type)) {
            let ty = type_.ty.strip_references();
            match ty.kind(Interner) {
                TyKind::Adt(_, substs) => {
                    cb(type_.derived(ty.clone()));
                    walk_substs(db, type_, substs, cb);
                }
                TyKind::AssociatedType(_, substs) => {
                    if ty.associated_type_parent_trait(db).is_some() {
                        cb(type_.derived(ty.clone()));
                    }
                    walk_substs(db, type_, substs, cb);
                }
                TyKind::OpaqueType(_, subst) => {
                    if let Some(bounds) = ty.impl_trait_bounds(db) {
                        walk_bounds(db, &type_.derived(ty.clone()), &bounds, cb);
                    }

                    walk_substs(db, type_, subst, cb);
                }
                TyKind::Alias(AliasTy::Opaque(opaque_ty)) => {
                    if let Some(bounds) = ty.impl_trait_bounds(db) {
                        walk_bounds(db, &type_.derived(ty.clone()), &bounds, cb);
                    }

                    walk_substs(db, type_, &opaque_ty.substitution, cb);
                }
                TyKind::Placeholder(_) => {
                    if let Some(bounds) = ty.impl_trait_bounds(db) {
                        walk_bounds(db, &type_.derived(ty.clone()), &bounds, cb);
                    }
                }
                TyKind::Dyn(bounds) => {
                    walk_bounds(
                        db,
                        &type_.derived(ty.clone()),
                        bounds.bounds.skip_binders().interned(),
                        cb,
                    );
                }

                TyKind::Ref(_, _, ty)
                | TyKind::Raw(_, ty)
                | TyKind::Array(ty, _)
                | TyKind::Slice(ty) => {
                    walk_type(db, &type_.derived(ty.clone()), cb);
                }

                TyKind::FnDef(_, substs)
                | TyKind::Tuple(_, substs)
                | TyKind::Closure(.., substs) => {
                    walk_substs(db, type_, substs, cb);
                }
                TyKind::Function(hir_ty::FnPointer { substitution, .. }) => {
                    walk_substs(db, type_, &substitution.0, cb);
                }

                _ => {}
            }
        }

        walk_type(db, self, &mut cb);
    }

    pub fn could_unify_with(&self, db: &dyn HirDatabase, other: &Type) -> bool {
        let tys = hir_ty::replace_errors_with_variables(&(self.ty.clone(), other.ty.clone()));
        hir_ty::could_unify(db, self.env.clone(), &tys)
    }

    pub fn could_coerce_to(&self, db: &dyn HirDatabase, to: &Type) -> bool {
        let tys = hir_ty::replace_errors_with_variables(&(self.ty.clone(), to.ty.clone()));
        hir_ty::could_coerce(db, self.env.clone(), &tys)
    }

    pub fn as_type_param(&self, db: &dyn HirDatabase) -> Option<TypeParam> {
        match self.ty.kind(Interner) {
            TyKind::Placeholder(p) => Some(TypeParam {
                id: TypeParamId::from_unchecked(hir_ty::from_placeholder_idx(db, *p)),
            }),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Callable {
    ty: Type,
    sig: CallableSig,
    callee: Callee,
    pub(crate) is_bound_method: bool,
}

#[derive(Debug)]
enum Callee {
    Def(CallableDefId),
    Closure(ClosureId),
    FnPtr,
}

pub enum CallableKind {
    Function(Function),
    TupleStruct(Struct),
    TupleEnumVariant(Variant),
    Closure,
    FnPtr,
}

impl Callable {
    pub fn kind(&self) -> CallableKind {
        use Callee::*;
        match self.callee {
            Def(CallableDefId::FunctionId(it)) => CallableKind::Function(it.into()),
            Def(CallableDefId::StructId(it)) => CallableKind::TupleStruct(it.into()),
            Def(CallableDefId::EnumVariantId(it)) => CallableKind::TupleEnumVariant(it.into()),
            Closure(_) => CallableKind::Closure,
            FnPtr => CallableKind::FnPtr,
        }
    }
    pub fn receiver_param(&self, db: &dyn HirDatabase) -> Option<ast::SelfParam> {
        let func = match self.callee {
            Callee::Def(CallableDefId::FunctionId(it)) if self.is_bound_method => it,
            _ => return None,
        };
        let src = func.lookup(db.upcast()).source(db.upcast());
        let param_list = src.value.param_list()?;
        param_list.self_param()
    }
    pub fn n_params(&self) -> usize {
        self.sig.params().len() - if self.is_bound_method { 1 } else { 0 }
    }
    pub fn params(
        &self,
        db: &dyn HirDatabase,
    ) -> Vec<(Option<Either<ast::SelfParam, ast::Pat>>, Type)> {
        let types = self
            .sig
            .params()
            .iter()
            .skip(if self.is_bound_method { 1 } else { 0 })
            .map(|ty| self.ty.derived(ty.clone()));
        let map_param = |it: ast::Param| it.pat().map(Either::Right);
        let patterns = match self.callee {
            Callee::Def(CallableDefId::FunctionId(func)) => {
                let src = func.lookup(db.upcast()).source(db.upcast());
                src.value.param_list().map(|param_list| {
                    param_list
                        .self_param()
                        .map(|it| Some(Either::Left(it)))
                        .filter(|_| !self.is_bound_method)
                        .into_iter()
                        .chain(param_list.params().map(map_param))
                })
            }
            Callee::Closure(closure_id) => match closure_source(db, closure_id) {
                Some(src) => src.param_list().map(|param_list| {
                    param_list
                        .self_param()
                        .map(|it| Some(Either::Left(it)))
                        .filter(|_| !self.is_bound_method)
                        .into_iter()
                        .chain(param_list.params().map(map_param))
                }),
                None => None,
            },
            _ => None,
        };
        patterns.into_iter().flatten().chain(iter::repeat(None)).zip(types).collect()
    }
    pub fn return_type(&self) -> Type {
        self.ty.derived(self.sig.ret().clone())
    }
}

fn closure_source(db: &dyn HirDatabase, closure: ClosureId) -> Option<ast::ClosureExpr> {
    let (owner, expr_id) = db.lookup_intern_closure(closure.into());
    let (_, source_map) = db.body_with_source_map(owner);
    let ast = source_map.expr_syntax(expr_id).ok()?;
    let root = ast.file_syntax(db.upcast());
    let expr = ast.value.to_node(&root);
    match expr {
        ast::Expr::ClosureExpr(it) => Some(it),
        _ => None,
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BindingMode {
    Move,
    Ref(Mutability),
}

/// For IDE only
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeDef {
    ModuleDef(ModuleDef),
    GenericParam(GenericParam),
    ImplSelfType(Impl),
    AdtSelfType(Adt),
    Local(Local),
    Label(Label),
    Unknown,
}

impl ScopeDef {
    pub fn all_items(def: PerNs) -> ArrayVec<Self, 3> {
        let mut items = ArrayVec::new();

        match (def.take_types(), def.take_values()) {
            (Some(m1), None) => items.push(ScopeDef::ModuleDef(m1.into())),
            (None, Some(m2)) => items.push(ScopeDef::ModuleDef(m2.into())),
            (Some(m1), Some(m2)) => {
                // Some items, like unit structs and enum variants, are
                // returned as both a type and a value. Here we want
                // to de-duplicate them.
                if m1 != m2 {
                    items.push(ScopeDef::ModuleDef(m1.into()));
                    items.push(ScopeDef::ModuleDef(m2.into()));
                } else {
                    items.push(ScopeDef::ModuleDef(m1.into()));
                }
            }
            (None, None) => {}
        };

        if let Some(macro_def_id) = def.take_macros() {
            items.push(ScopeDef::ModuleDef(ModuleDef::Macro(macro_def_id.into())));
        }

        if items.is_empty() {
            items.push(ScopeDef::Unknown);
        }

        items
    }

    pub fn attrs(&self, db: &dyn HirDatabase) -> Option<AttrsWithOwner> {
        match self {
            ScopeDef::ModuleDef(it) => it.attrs(db),
            ScopeDef::GenericParam(it) => Some(it.attrs(db)),
            ScopeDef::ImplSelfType(_)
            | ScopeDef::AdtSelfType(_)
            | ScopeDef::Local(_)
            | ScopeDef::Label(_)
            | ScopeDef::Unknown => None,
        }
    }

    pub fn krate(&self, db: &dyn HirDatabase) -> Option<Crate> {
        match self {
            ScopeDef::ModuleDef(it) => it.module(db).map(|m| m.krate()),
            ScopeDef::GenericParam(it) => Some(it.module(db).krate()),
            ScopeDef::ImplSelfType(_) => None,
            ScopeDef::AdtSelfType(it) => Some(it.module(db).krate()),
            ScopeDef::Local(it) => Some(it.module(db).krate()),
            ScopeDef::Label(it) => Some(it.module(db).krate()),
            ScopeDef::Unknown => None,
        }
    }
}

impl From<ItemInNs> for ScopeDef {
    fn from(item: ItemInNs) -> Self {
        match item {
            ItemInNs::Types(id) => ScopeDef::ModuleDef(id),
            ItemInNs::Values(id) => ScopeDef::ModuleDef(id),
            ItemInNs::Macros(id) => ScopeDef::ModuleDef(ModuleDef::Macro(id)),
        }
    }
}

pub trait HasVisibility {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility;
    fn is_visible_from(&self, db: &dyn HirDatabase, module: Module) -> bool {
        let vis = self.visibility(db);
        vis.is_visible_from(db.upcast(), module.id)
    }
}

/// Trait for obtaining the defining crate of an item.
pub trait HasCrate {
    fn krate(&self, db: &dyn HirDatabase) -> Crate;
}

impl<T: hir_def::HasModule> HasCrate for T {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db.upcast()).krate().into()
    }
}

impl HasCrate for AssocItem {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Struct {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Union {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Field {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.parent_def(db).module(db).krate()
    }
}

impl HasCrate for Variant {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Function {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Const {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for TypeAlias {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Type {
    fn krate(&self, _db: &dyn HirDatabase) -> Crate {
        self.env.krate.into()
    }
}

impl HasCrate for Macro {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Trait {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Static {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Adt {
    fn krate(&self, db: &dyn HirDatabase) -> Crate {
        self.module(db).krate()
    }
}

impl HasCrate for Module {
    fn krate(&self, _: &dyn HirDatabase) -> Crate {
        Module::krate(*self)
    }
}
