// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    diag,
    diagnostics::{codes::WarningFilter, Diagnostic, WarningFilters},
    editions::FeatureGate,
    expansion::{
        aliases::{AliasMap, AliasSet},
        ast::{self as E, Address, Fields, ModuleIdent, ModuleIdent_, SpecId},
        byte_string, hex_string,
    },
    parser::ast::{
        self as P, Ability, ConstantName, Field, FieldBindings, FunctionName, ModuleName,
        Mutability, StructName, Var,
    },
    shared::{known_attributes::AttributePosition, unique_map::UniqueMap, *},
    FullyCompiledProgram,
};
use move_command_line_common::parser::{parse_u16, parse_u256, parse_u32};
use move_core_types::account_address::AccountAddress;
use move_ir_types::location::*;
use move_symbol_pool::Symbol;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    iter::IntoIterator,
};

use super::aliases::{AliasMapBuilder, OldAliasMap, ParserExplicitUseFun, UseFunsBuilder};

//**************************************************************************************************
// Context
//**************************************************************************************************

type ModuleMembers = BTreeMap<Name, ModuleMemberKind>;

struct Context<'env, 'map> {
    module_members: UniqueMap<ModuleIdent, ModuleMembers>,
    named_address_mapping: Option<&'map NamedAddressMap>,
    address_conflicts: BTreeSet<Symbol>,
    address: Option<Address>,
    aliases: AliasMap,
    is_source_definition: bool,
    current_package: Option<Symbol>,
    in_spec_context: bool,
    exp_specs: BTreeMap<SpecId, E::SpecBlock>,
    // Cached warning filters for all available prefixes. Used by non-source defs
    // and dependency packages
    all_filter_alls: WarningFilters,
    env: &'env mut CompilationEnv,
}

impl<'env, 'map> Context<'env, 'map> {
    fn new(
        compilation_env: &'env mut CompilationEnv,
        module_members: UniqueMap<ModuleIdent, ModuleMembers>,
        address_conflicts: BTreeSet<Symbol>,
    ) -> Self {
        let mut all_filter_alls = WarningFilters::new_for_dependency();
        for allow in compilation_env.filter_attributes() {
            for f in compilation_env.filter_from_str(FILTER_ALL, *allow) {
                all_filter_alls.add(f);
            }
        }
        Self {
            module_members,
            env: compilation_env,
            named_address_mapping: None,
            address_conflicts,
            address: None,
            aliases: AliasMap::new(),
            is_source_definition: false,
            current_package: None,
            in_spec_context: false,
            exp_specs: BTreeMap::new(),
            all_filter_alls,
        }
    }

    fn cur_address(&self) -> &Address {
        self.address.as_ref().unwrap()
    }

    /// Resets the alias map and reports errors for aliases that were unused
    /// Marks implicit use funs as unused
    pub fn set_to_outer_scope(
        &mut self,
        mut use_funs: Option<&mut E::UseFuns>,
        outer_scope: OldAliasMap,
    ) {
        let AliasSet { modules, members } = self.aliases.set_to_outer_scope(outer_scope);
        for alias in modules {
            unused_alias(self, alias)
        }
        for alias in members {
            let use_fun_used_opt = use_funs
                .as_mut()
                .and_then(|use_funs| use_funs.implicit.get_mut(&alias))
                .and_then(|use_fun| match &mut use_fun.kind {
                    E::ImplicitUseFunKind::FunctionDeclaration => None,
                    E::ImplicitUseFunKind::UseAlias { used } => Some(used),
                });
            if let Some(used) = use_fun_used_opt {
                // We do not report the use error if it is a function alias, since these will be
                // reported after method calls are fully resolved
                *used = false;
            } else {
                unused_alias(self, alias)
            }
        }
    }

    pub fn bind_exp_spec(&mut self, spec_block: P::SpecBlock) -> (SpecId, BTreeSet<Name>) {
        let len = self.exp_specs.len();
        let id = SpecId::new(len);
        let espec_block = spec(self, spec_block);
        let mut unbound_names = BTreeSet::new();
        unbound_names_spec_block(&mut unbound_names, &espec_block);
        self.exp_specs.insert(id, espec_block);

        (id, unbound_names)
    }

    pub fn extract_exp_specs(&mut self) -> BTreeMap<SpecId, E::SpecBlock> {
        std::mem::take(&mut self.exp_specs)
    }
}

/// We mark named addresses as having a conflict if there is not a bidirectional mapping between
/// the name and its value
fn compute_address_conflicts(
    pre_compiled_lib: Option<&FullyCompiledProgram>,
    prog: &P::Program,
) -> BTreeSet<Symbol> {
    let mut name_to_addr: BTreeMap<Symbol, BTreeSet<AccountAddress>> = BTreeMap::new();
    let mut addr_to_name: BTreeMap<AccountAddress, BTreeSet<Symbol>> = BTreeMap::new();
    let all_addrs = prog.named_address_maps.all().iter().chain(
        pre_compiled_lib
            .iter()
            .flat_map(|pre| pre.parser.named_address_maps.all()),
    );
    for map in all_addrs {
        for (n, addr) in map {
            let n = *n;
            let addr = addr.into_inner();
            name_to_addr.entry(n).or_default().insert(addr);
            addr_to_name.entry(addr).or_default().insert(n);
        }
    }
    let name_to_addr_conflicts = name_to_addr
        .into_iter()
        .filter(|(_, addrs)| addrs.len() > 1)
        .map(|(n, _)| n);
    let addr_to_name_conflicts = addr_to_name
        .into_iter()
        .filter(|(_, addrs)| addrs.len() > 1)
        .flat_map(|(_, ns)| ns.into_iter());
    name_to_addr_conflicts
        .chain(addr_to_name_conflicts)
        .collect()
}

//**************************************************************************************************
// Entry
//**************************************************************************************************

pub fn program(
    compilation_env: &mut CompilationEnv,
    pre_compiled_lib: Option<&FullyCompiledProgram>,
    prog: P::Program,
) -> E::Program {
    let address_conflicts = compute_address_conflicts(pre_compiled_lib, &prog);
    let module_members = {
        let mut members = UniqueMap::new();
        all_module_members(
            compilation_env,
            &address_conflicts,
            &prog.named_address_maps,
            &mut members,
            true,
            &prog.source_definitions,
        );
        all_module_members(
            compilation_env,
            &address_conflicts,
            &prog.named_address_maps,
            &mut members,
            true,
            &prog.lib_definitions,
        );
        if let Some(pre_compiled) = pre_compiled_lib {
            assert!(pre_compiled.parser.lib_definitions.is_empty());
            all_module_members(
                compilation_env,
                &address_conflicts,
                &pre_compiled.parser.named_address_maps,
                &mut members,
                false,
                &pre_compiled.parser.source_definitions,
            );
        }
        members
    };

    let mut context = Context::new(compilation_env, module_members, address_conflicts);

    let mut source_module_map = UniqueMap::new();
    let mut lib_module_map = UniqueMap::new();
    let mut scripts = vec![];
    let P::Program {
        named_address_maps,
        source_definitions,
        lib_definitions,
    } = prog;

    context.is_source_definition = true;
    for P::PackageDefinition {
        package,
        named_address_map,
        def,
    } in source_definitions
    {
        context.current_package = package;
        context.named_address_mapping = Some(named_address_maps.get(named_address_map));
        definition(
            &mut context,
            &mut source_module_map,
            &mut scripts,
            package,
            def,
        )
    }

    context.is_source_definition = false;
    for P::PackageDefinition {
        package,
        named_address_map,
        def,
    } in lib_definitions
    {
        context.current_package = package;
        context.named_address_mapping = Some(named_address_maps.get(named_address_map));
        definition(
            &mut context,
            &mut lib_module_map,
            &mut scripts,
            package,
            def,
        )
    }
    context.current_package = None;

    for (mident, module) in lib_module_map {
        if let Err((mident, old_loc)) = source_module_map.add(mident, module) {
            if !context.env.flags().sources_shadow_deps() {
                duplicate_module(&mut context, &source_module_map, mident, old_loc)
            }
        }
    }
    let module_map = source_module_map;

    let scripts = {
        let mut collected: BTreeMap<Symbol, Vec<E::Script>> = BTreeMap::new();
        for s in scripts {
            collected
                .entry(s.function_name.value())
                .or_default()
                .push(s)
        }
        let mut keyed: BTreeMap<Symbol, E::Script> = BTreeMap::new();
        for (n, mut ss) in collected {
            match ss.len() {
                0 => unreachable!(),
                1 => assert!(
                    keyed.insert(n, ss.pop().unwrap()).is_none(),
                    "ICE duplicate script key"
                ),
                _ => {
                    for (i, s) in ss.into_iter().enumerate() {
                        let k = format!("{}_{}", n, i);
                        assert!(
                            keyed.insert(k.into(), s).is_none(),
                            "ICE duplicate script key"
                        )
                    }
                }
            }
        }
        keyed
    };

    super::primitive_definers::modules(context.env, pre_compiled_lib, &module_map);
    E::Program {
        modules: module_map,
        scripts,
    }
}

fn definition(
    context: &mut Context,
    module_map: &mut UniqueMap<ModuleIdent, E::ModuleDefinition>,
    scripts: &mut Vec<E::Script>,
    package_name: Option<Symbol>,
    def: P::Definition,
) {
    match def {
        P::Definition::Module(mut m) => {
            let module_paddr = std::mem::take(&mut m.address);
            let module_addr = module_paddr
                .map(|a| sp(a.loc, address(context, /* suggest_declaration */ true, a)));
            module(context, module_map, package_name, module_addr, m)
        }
        P::Definition::Address(a) => {
            let addr = address(context, /* suggest_declaration */ false, a.addr);
            for mut m in a.modules {
                let module_addr = check_module_address(context, a.loc, addr, &mut m);
                module(context, module_map, package_name, Some(module_addr), m)
            }
        }

        P::Definition::Script(_) if !context.is_source_definition => (),
        P::Definition::Script(s) => script(context, scripts, package_name, s),
    }
}

fn address_without_value_error(suggest_declaration: bool, loc: Loc, n: &Name) -> Diagnostic {
    let mut msg = format!("address '{}' is not assigned a value", n);
    if suggest_declaration {
        msg = format!(
            "{}. Try assigning it a value when calling the compiler",
            msg,
        )
    }
    diag!(NameResolution::AddressWithoutValue, (loc, msg))
}

// Access a top level address as declared, not affected by any aliasing/shadowing
fn address(context: &mut Context, suggest_declaration: bool, ln: P::LeadingNameAccess) -> Address {
    address_(
        context.env,
        &context.address_conflicts,
        context.named_address_mapping.as_ref().unwrap(),
        suggest_declaration,
        ln,
    )
}

fn address_(
    compilation_env: &mut CompilationEnv,
    address_conflicts: &BTreeSet<Symbol>,
    named_address_mapping: &NamedAddressMap,
    suggest_declaration: bool,
    ln: P::LeadingNameAccess,
) -> Address {
    let name_res = check_valid_address_name_(compilation_env, &ln);
    let sp!(loc, ln_) = ln;
    match ln_ {
        P::LeadingNameAccess_::AnonymousAddress(bytes) => {
            debug_assert!(name_res.is_ok());
            Address::anonymous(loc, bytes)
        }
        P::LeadingNameAccess_::Name(n) => match named_address_mapping.get(&n.value).copied() {
            Some(addr) => Address::Numerical {
                name: Some(n),
                value: sp(loc, addr),
                name_conflict: address_conflicts.contains(&n.value),
            },
            None => {
                if name_res.is_ok() {
                    compilation_env.add_diag(address_without_value_error(
                        suggest_declaration,
                        loc,
                        &n,
                    ));
                }
                Address::NamedUnassigned(n)
            }
        },
    }
}

fn module_ident(context: &mut Context, sp!(loc, mident_): P::ModuleIdent) -> ModuleIdent {
    let P::ModuleIdent_ {
        address: ln,
        module,
    } = mident_;
    let addr = address(context, /* suggest_declaration */ false, ln);
    sp(loc, ModuleIdent_::new(addr, module))
}

fn check_module_address(
    context: &mut Context,
    loc: Loc,
    addr: Address,
    m: &mut P::ModuleDefinition,
) -> Spanned<Address> {
    let module_address = std::mem::take(&mut m.address);
    match module_address {
        Some(other_paddr) => {
            let other_loc = other_paddr.loc;
            let other_addr = address(context, /* suggest_declaration */ true, other_paddr);
            let msg = if addr == other_addr {
                "Redundant address specification"
            } else {
                "Multiple addresses specified for module"
            };
            context.env.add_diag(diag!(
                Declarations::DuplicateItem,
                (other_loc, msg),
                (loc, "Address previously specified here")
            ));
            sp(other_loc, other_addr)
        }
        None => sp(loc, addr),
    }
}

fn duplicate_module(
    context: &mut Context,
    module_map: &UniqueMap<ModuleIdent, E::ModuleDefinition>,
    mident: ModuleIdent,
    old_loc: Loc,
) {
    let old_mident = module_map.get_key(&mident).unwrap();
    let dup_msg = format!("Duplicate definition for module '{}'", mident);
    let prev_msg = format!("Module previously defined here, with '{}'", old_mident);
    context.env.add_diag(diag!(
        Declarations::DuplicateItem,
        (mident.loc, dup_msg),
        (old_loc, prev_msg),
    ))
}

fn module(
    context: &mut Context,
    module_map: &mut UniqueMap<ModuleIdent, E::ModuleDefinition>,
    package_name: Option<Symbol>,
    module_address: Option<Spanned<Address>>,
    module_def: P::ModuleDefinition,
) {
    assert!(context.address.is_none());
    let (mident, mod_) = module_(context, package_name, module_address, module_def);
    if let Err((mident, old_loc)) = module_map.add(mident, mod_) {
        duplicate_module(context, module_map, mident, old_loc)
    }
    context.address = None
}

fn set_sender_address(
    context: &mut Context,
    module_name: &ModuleName,
    sender: Option<Spanned<Address>>,
) {
    context.address = Some(match sender {
        Some(sp!(_, addr)) => addr,
        None => {
            let loc = module_name.loc();
            let msg = format!(
                "Invalid module declaration. The module does not have a specified address. Either \
                 declare it inside of an 'address <address> {{' block or declare it with an \
                 address 'module <address>::{}''",
                module_name
            );
            context
                .env
                .add_diag(diag!(Declarations::InvalidModule, (loc, msg)));
            Address::anonymous(loc, NumericalAddress::DEFAULT_ERROR_ADDRESS)
        }
    })
}

fn module_(
    context: &mut Context,
    package_name: Option<Symbol>,
    module_address: Option<Spanned<Address>>,
    mdef: P::ModuleDefinition,
) -> (ModuleIdent, E::ModuleDefinition) {
    let P::ModuleDefinition {
        attributes,
        loc,
        address,
        is_spec_module: _,
        name,
        members,
    } = mdef;
    let attributes = flatten_attributes(context, AttributePosition::Module, attributes);
    let mut warning_filter = module_warning_filter(context, &attributes);
    let config = context.env.package_config(package_name);
    warning_filter.union(&config.warning_filter);

    context.env.add_warning_filter_scope(warning_filter.clone());
    assert!(context.address.is_none());
    assert!(address.is_none());
    set_sender_address(context, &name, module_address);
    let _ = check_restricted_name_all_cases(context, NameCase::Module, &name.0);
    if name.value().starts_with(|c| c == '_') {
        let msg = format!(
            "Invalid module name '{}'. Module names cannot start with '_'",
            name,
        );
        context
            .env
            .add_diag(diag!(Declarations::InvalidName, (name.loc(), msg)));
    }

    let name_loc = name.0.loc;
    let current_module = sp(name_loc, ModuleIdent_::new(*context.cur_address(), name));

    let mut new_scope = AliasMapBuilder::new();
    let mut use_funs_builder = UseFunsBuilder::new();
    module_self_aliases(&mut new_scope, &current_module);
    let members = members
        .into_iter()
        .filter_map(|member| {
            aliases_from_member(
                context,
                &mut new_scope,
                &mut use_funs_builder,
                &current_module,
                member,
            )
        })
        .collect::<Vec<_>>();
    let old_aliases = context.aliases.add_and_shadow_all(new_scope);
    assert!(
        old_aliases.is_empty(),
        "ICE there should be no aliases entering a module"
    );

    let mut friends = UniqueMap::new();
    let mut functions = UniqueMap::new();
    let mut constants = UniqueMap::new();
    let mut structs = UniqueMap::new();
    let mut specs = vec![];
    for member in members {
        match member {
            P::ModuleMember::Use(_) => unreachable!(),
            P::ModuleMember::Friend(f) => friend(context, &mut friends, f),
            P::ModuleMember::Function(mut f) => {
                if !context.is_source_definition {
                    f.body.value = P::FunctionBody_::Native
                }
                function(
                    context,
                    Some((current_module, &mut use_funs_builder)),
                    &mut functions,
                    f,
                )
            }
            P::ModuleMember::Constant(c) => constant(context, &mut constants, c),
            P::ModuleMember::Struct(s) => struct_def(context, &mut structs, s),
            P::ModuleMember::Spec(s) => specs.push(spec(context, s)),
        }
    }
    let mut use_funs = use_funs(context, use_funs_builder);
    check_visibility_modifiers(context, &functions, &friends, package_name);

    context.set_to_outer_scope(Some(&mut use_funs), old_aliases);

    let def = E::ModuleDefinition {
        package_name,
        attributes,
        loc,
        use_funs,
        is_source_module: context.is_source_definition,
        friends,
        structs,
        constants,
        functions,
        specs,
        warning_filter,
    };
    context.env.pop_warning_filter_scope();
    (current_module, def)
}

fn check_visibility_modifiers(
    context: &mut Context,
    functions: &UniqueMap<FunctionName, E::Function>,
    friends: &UniqueMap<ModuleIdent, E::Friend>,
    package_name: Option<Symbol>,
) {
    let mut friend_usage = friends.iter().next().map(|(_, _, friend)| friend.loc);
    let mut public_package_usage = None;
    for (_, _, function) in functions {
        match function.visibility {
            E::Visibility::Friend(loc) if friend_usage.is_none() => {
                friend_usage = Some(loc);
            }
            E::Visibility::Package(loc) => {
                context
                    .env
                    .check_feature(FeatureGate::PublicPackage, package_name, loc);
                public_package_usage = Some(loc);
            }
            _ => (),
        }
    }

    // Emit any errors.
    if public_package_usage.is_some() && friend_usage.is_some() {
        let friend_error_msg = format!(
            "Cannot define 'friend' modules and use '{}' visibility in the same module",
            E::Visibility::PACKAGE
        );
        let package_definition_msg = format!("'{}' visibility used here", E::Visibility::PACKAGE);
        for (_, _, friend) in friends {
            context.env.add_diag(diag!(
                Declarations::InvalidVisibilityModifier,
                (friend.loc, friend_error_msg.clone()),
                (
                    public_package_usage.unwrap(),
                    package_definition_msg.clone()
                )
            ));
        }
        let package_error_msg = format!(
            "Cannot mix '{}' and '{}' visibilities in the same module",
            E::Visibility::PACKAGE_IDENT,
            E::Visibility::FRIEND_IDENT
        );
        let friend_error_msg = format!(
            "Cannot mix '{}' and '{}' visibilities in the same module",
            E::Visibility::FRIEND_IDENT,
            E::Visibility::PACKAGE_IDENT
        );
        for (_, _, function) in functions {
            match function.visibility {
                E::Visibility::Friend(loc) => {
                    context.env.add_diag(diag!(
                        Declarations::InvalidVisibilityModifier,
                        (loc, friend_error_msg.clone()),
                        (
                            public_package_usage.unwrap(),
                            package_definition_msg.clone()
                        )
                    ));
                }
                E::Visibility::Package(loc) => {
                    context.env.add_diag(diag!(
                        Declarations::InvalidVisibilityModifier,
                        (loc, package_error_msg.clone()),
                        (
                            friend_usage.unwrap(),
                            &format!("'{}' visibility used here", E::Visibility::FRIEND_IDENT)
                        )
                    ));
                }
                _ => {}
            }
        }
    }
}

fn script(
    context: &mut Context,
    scripts: &mut Vec<E::Script>,
    package_name: Option<Symbol>,
    pscript: P::Script,
) {
    scripts.push(script_(context, package_name, pscript))
}

fn script_(context: &mut Context, package_name: Option<Symbol>, pscript: P::Script) -> E::Script {
    assert!(context.address.is_none());
    assert!(context.is_source_definition);
    let P::Script {
        attributes,
        loc,
        uses: puses,
        constants: pconstants,
        function: pfunction,
        specs: pspecs,
    } = pscript;

    let attributes = flatten_attributes(context, AttributePosition::Script, attributes);
    let mut warning_filter = warning_filter(context, &attributes);
    let config = context.env.package_config(package_name);
    warning_filter.union(&config.warning_filter);

    context.env.add_warning_filter_scope(warning_filter.clone());
    let (new_scope, use_funs_builder) = uses(context, puses);
    let old_aliases = context.aliases.add_and_shadow_all(new_scope);
    assert!(
        old_aliases.is_empty(),
        "ICE there should be no aliases entering a script"
    );
    let mut use_funs = use_funs(context, use_funs_builder);

    let mut constants = UniqueMap::new();
    for c in pconstants {
        // TODO remove after Self rework
        check_valid_module_member_name(context, ModuleMemberKind::Constant, c.name.0);
        constant(context, &mut constants, c);
    }

    // TODO remove after Self rework
    check_valid_module_member_name(context, ModuleMemberKind::Function, pfunction.name.0);
    let (function_name, function) = function_(context, None, 0, pfunction);
    match &function.visibility {
        E::Visibility::Friend(loc) | E::Visibility::Package(loc) | E::Visibility::Public(loc) => {
            let msg = format!(
                "Invalid '{}' visibility modifier. \
                Script functions are not callable from other Move functions.",
                function.visibility,
            );
            context
                .env
                .add_diag(diag!(Declarations::UnnecessaryItem, (*loc, msg)));
        }
        E::Visibility::Internal => (),
    }
    match &function.body {
        sp!(_, E::FunctionBody_::Defined(_)) => (),
        sp!(loc, E::FunctionBody_::Native) => {
            context.env.add_diag(diag!(
                Declarations::InvalidScript,
                (
                    *loc,
                    "Invalid 'native' function. 'script' functions must have a defined body"
                )
            ));
        }
    }
    let specs = specs(context, pspecs);
    context.set_to_outer_scope(Some(&mut use_funs), old_aliases);
    context.env.pop_warning_filter_scope();
    E::Script {
        warning_filter,
        package_name,
        attributes,
        loc,
        use_funs,
        constants,
        function_name,
        function,
        specs,
    }
}

fn flatten_attributes(
    context: &mut Context,
    attr_position: AttributePosition,
    attributes: Vec<P::Attributes>,
) -> E::Attributes {
    let all_attrs = attributes
        .into_iter()
        .flat_map(|attrs| attrs.value)
        .flat_map(|attr| attribute(context, attr_position, attr))
        .collect::<Vec<_>>();
    unique_attributes(context, attr_position, false, all_attrs)
}

fn unique_attributes(
    context: &mut Context,
    attr_position: AttributePosition,
    is_nested: bool,
    attributes: impl IntoIterator<Item = E::Attribute>,
) -> E::Attributes {
    let mut attr_map = UniqueMap::new();
    for sp!(loc, attr_) in attributes {
        let sp!(nloc, sym) = match &attr_ {
            E::Attribute_::Name(n)
            | E::Attribute_::Assigned(n, _)
            | E::Attribute_::Parameterized(n, _) => *n,
        };
        let name_ = match known_attributes::KnownAttribute::resolve(sym) {
            None => E::AttributeName_::Unknown(sym),
            Some(known) => {
                debug_assert!(known.name() == sym.as_str());
                if is_nested {
                    let msg = "Known attribute '{}' is not expected in a nested attribute position";
                    context
                        .env
                        .add_diag(diag!(Declarations::InvalidAttribute, (nloc, msg)));
                    continue;
                }

                let expected_positions = known.expected_positions();
                if !expected_positions.contains(&attr_position) {
                    let msg = format!(
                        "Known attribute '{}' is not expected with a {}",
                        known.name(),
                        attr_position
                    );
                    let all_expected = expected_positions
                        .iter()
                        .map(|p| format!("{}", p))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let expected_msg = format!(
                        "Expected to be used with one of the following: {}",
                        all_expected
                    );
                    context.env.add_diag(diag!(
                        Declarations::InvalidAttribute,
                        (nloc, msg),
                        (nloc, expected_msg)
                    ));
                    continue;
                }
                E::AttributeName_::Known(known)
            }
        };
        if let Err((_, old_loc)) = attr_map.add(sp(nloc, name_), sp(loc, attr_)) {
            let msg = format!("Duplicate attribute '{}' attached to the same item", name_);
            context.env.add_diag(diag!(
                Declarations::DuplicateItem,
                (loc, msg),
                (old_loc, "Attribute previously given here"),
            ));
        }
    }
    attr_map
}

fn attribute(
    context: &mut Context,
    attr_position: AttributePosition,
    sp!(loc, attribute_): P::Attribute,
) -> Option<E::Attribute> {
    use E::Attribute_ as EA;
    use P::Attribute_ as PA;
    Some(sp(
        loc,
        match attribute_ {
            PA::Name(n) => EA::Name(n),
            PA::Assigned(n, v) => EA::Assigned(n, Box::new(attribute_value(context, *v)?)),
            PA::Parameterized(n, sp!(_, pattrs_)) => {
                let attrs = pattrs_
                    .into_iter()
                    .map(|a| attribute(context, attr_position, a))
                    .collect::<Option<Vec<_>>>()?;
                EA::Parameterized(n, unique_attributes(context, attr_position, true, attrs))
            }
        },
    ))
}

fn attribute_value(
    context: &mut Context,
    sp!(loc, avalue_): P::AttributeValue,
) -> Option<E::AttributeValue> {
    use E::AttributeValue_ as EV;
    use P::{AttributeValue_ as PV, LeadingNameAccess_ as LN, NameAccessChain_ as PN};
    Some(sp(
        loc,
        match avalue_ {
            PV::Value(v) => EV::Value(value(context, v)?),
            PV::ModuleAccess(sp!(ident_loc, PN::Two(sp!(aloc, LN::AnonymousAddress(a)), n))) => {
                let addr = Address::anonymous(aloc, a);
                let mident = sp(ident_loc, ModuleIdent_::new(addr, ModuleName(n)));
                if context.module_members.get(&mident).is_none() {
                    context.env.add_diag(diag!(
                        NameResolution::UnboundModule,
                        (ident_loc, format!("Unbound module '{}'", mident))
                    ));
                }
                EV::Module(mident)
            }
            // bit wonky, but this is the only spot currently where modules and expressions exist
            // in the same namespace.
            // TODO consider if we want to just force all of these checks into the well-known
            // attribute setup
            PV::ModuleAccess(sp!(ident_loc, PN::One(n)))
                if context.aliases.module_alias_get(&n).is_some() =>
            {
                let sp!(_, mident_) = context.aliases.module_alias_get(&n).unwrap();
                let mident = sp(ident_loc, mident_);
                if context.module_members.get(&mident).is_none() {
                    context.env.add_diag(diag!(
                        NameResolution::UnboundModule,
                        (ident_loc, format!("Unbound module '{}'", mident))
                    ));
                }
                EV::Module(mident)
            }
            PV::ModuleAccess(sp!(ident_loc, PN::Two(sp!(aloc, LN::Name(n1)), n2)))
                if context
                    .named_address_mapping
                    .as_ref()
                    .map(|m| m.contains_key(&n1.value))
                    .unwrap_or(false) =>
            {
                let addr = address(context, false, sp(aloc, LN::Name(n1)));
                let mident = sp(ident_loc, ModuleIdent_::new(addr, ModuleName(n2)));
                if context.module_members.get(&mident).is_none() {
                    context.env.add_diag(diag!(
                        NameResolution::UnboundModule,
                        (ident_loc, format!("Unbound module '{}'", mident))
                    ));
                }
                EV::Module(mident)
            }
            PV::ModuleAccess(ma) => EV::ModuleAccess(name_access_chain(context, Access::Type, ma)?),
        },
    ))
}

/// Like warning_filter, but it will filter _all_ warnings for non-source definitions (or for any
/// dependency packages)
fn module_warning_filter(
    context: &mut Context,
    attributes: &UniqueMap<E::AttributeName, E::Attribute>,
) -> WarningFilters {
    let filters = warning_filter(context, attributes);
    let is_dep = !context.is_source_definition
        || context
            .env
            .package_config(context.current_package)
            .is_dependency;
    if is_dep {
        // For dependencies (non source defs or package deps), we check the filters for errors
        // but then throw them away and actually ignore _all_ warnings
        context.all_filter_alls.clone()
    } else {
        filters
    }
}

fn warning_filter(
    context: &mut Context,
    attributes: &UniqueMap<E::AttributeName, E::Attribute>,
) -> WarningFilters {
    use crate::diagnostics::codes::Category;
    use known_attributes::DiagnosticAttribute;
    let mut warning_filters = WarningFilters::new_for_source();
    let filter_attribute_names = context.env.filter_attributes().clone();
    for allow in filter_attribute_names {
        let Some(attr) = attributes.get_(&allow) else {
            continue;
        };
        let inners = match &attr.value {
            E::Attribute_::Parameterized(_, inner) if !inner.is_empty() => inner,
            _ => {
                let msg = format!(
                    "Expected list of warnings, e.g. '{}({})'",
                    DiagnosticAttribute::ALLOW,
                    WarningFilter::Category {
                        prefix: None,
                        category: Category::UnusedItem as u8,
                        name: Some(FILTER_UNUSED)
                    }
                    .to_str()
                    .unwrap(),
                );
                context
                    .env
                    .add_diag(diag!(Attributes::InvalidValue, (attr.loc, msg)));
                continue;
            }
        };
        for (inner_attr_loc, _, inner_attr) in inners {
            let sp!(_, name_) = match inner_attr.value {
                E::Attribute_::Name(n) => n,
                E::Attribute_::Assigned(n, _) | E::Attribute_::Parameterized(n, _) => {
                    let msg = format!(
                        "Expected a stand alone warning filter identifier, e.g. '{}({})'",
                        DiagnosticAttribute::ALLOW,
                        n
                    );
                    context
                        .env
                        .add_diag(diag!(Attributes::InvalidValue, (inner_attr_loc, msg)));
                    n
                }
            };
            let filters = context.env.filter_from_str(name_, allow);
            if filters.is_empty() {
                let msg = format!("Unknown warning filter '{name_}'");
                context
                    .env
                    .add_diag(diag!(Attributes::InvalidValue, (attr.loc, msg)));
                continue;
            };
            for f in filters {
                warning_filters.add(f);
            }
        }
    }
    warning_filters
}

//**************************************************************************************************
// Aliases
//**************************************************************************************************

fn all_module_members<'a>(
    compilation_env: &mut CompilationEnv,
    address_conflicts: &BTreeSet<Symbol>,
    named_addr_maps: &NamedAddressMaps,
    members: &mut UniqueMap<ModuleIdent, ModuleMembers>,
    always_add: bool,
    defs: impl IntoIterator<Item = &'a P::PackageDefinition>,
) {
    for P::PackageDefinition {
        named_address_map,
        def,
        ..
    } in defs
    {
        let named_addr_map = named_addr_maps.get(*named_address_map);
        match def {
            P::Definition::Module(m) => {
                let addr = match &m.address {
                    Some(a) => {
                        address_(
                            compilation_env,
                            address_conflicts,
                            named_addr_map,
                            /* suggest_declaration */ true,
                            *a,
                        )
                    }
                    // Error will be handled when the module is compiled
                    None => Address::anonymous(m.loc, NumericalAddress::DEFAULT_ERROR_ADDRESS),
                };
                module_members(members, always_add, addr, m)
            }
            P::Definition::Address(addr_def) => {
                let addr = address_(
                    compilation_env,
                    address_conflicts,
                    named_addr_map,
                    /* suggest_declaration */ false,
                    addr_def.addr,
                );
                for m in &addr_def.modules {
                    module_members(members, always_add, addr, m)
                }
            }
            P::Definition::Script(_) => (),
        }
    }
}

fn module_members(
    members: &mut UniqueMap<ModuleIdent, ModuleMembers>,
    always_add: bool,
    address: Address,
    m: &P::ModuleDefinition,
) {
    let mident = sp(m.name.loc(), ModuleIdent_::new(address, m.name));
    if !always_add && members.contains_key(&mident) {
        return;
    }
    let mut cur_members = members.remove(&mident).unwrap_or_default();
    for mem in &m.members {
        use P::{SpecBlockMember_ as SBM, SpecBlockTarget_ as SBT, SpecBlock_ as SB};
        match mem {
            P::ModuleMember::Function(f) => {
                cur_members.insert(f.name.0, ModuleMemberKind::Function);
            }
            P::ModuleMember::Constant(c) => {
                cur_members.insert(c.name.0, ModuleMemberKind::Constant);
            }
            P::ModuleMember::Struct(s) => {
                cur_members.insert(s.name.0, ModuleMemberKind::Struct);
            }
            P::ModuleMember::Spec(
                sp!(
                    _,
                    SB {
                        target,
                        members,
                        ..
                    }
                ),
            ) => match &target.value {
                SBT::Schema(n, _) => {
                    cur_members.insert(*n, ModuleMemberKind::Schema);
                }
                SBT::Module => {
                    for sp!(_, smember_) in members {
                        if let SBM::Function { name, .. } = smember_ {
                            cur_members.insert(name.0, ModuleMemberKind::Function);
                        }
                    }
                }
                _ => (),
            },
            P::ModuleMember::Use(_) | P::ModuleMember::Friend(_) => (),
        };
    }
    members.add(mident, cur_members).unwrap();
}

fn module_self_aliases(acc: &mut AliasMapBuilder, current_module: &ModuleIdent) {
    let self_name = sp(current_module.loc, ModuleName::SELF_NAME.into());
    acc.add_implicit_module_alias(self_name, *current_module)
        .unwrap()
}

fn aliases_from_member(
    context: &mut Context,
    acc: &mut AliasMapBuilder,
    use_funs: &mut UseFunsBuilder,
    current_module: &ModuleIdent,
    member: P::ModuleMember,
) -> Option<P::ModuleMember> {
    use P::{SpecBlockMember_ as SBM, SpecBlockTarget_ as SBT, SpecBlock_ as SB};
    macro_rules! check_name_and_add_implicit_alias {
        ($kind:expr, $name:expr) => {{
            if let Some(n) = check_valid_module_member_name(context, $kind, $name) {
                if let Err(loc) =
                    acc.add_implicit_member_alias(n.clone(), current_module.clone(), n.clone())
                {
                    duplicate_module_member(context, loc, n)
                }
            }
        }};
    }

    match member {
        P::ModuleMember::Use(u) => {
            use_(context, acc, use_funs, u);
            None
        }
        f @ P::ModuleMember::Friend(_) => {
            // friend declarations do not produce implicit aliases
            Some(f)
        }
        P::ModuleMember::Function(f) => {
            let n = f.name.0;
            check_name_and_add_implicit_alias!(ModuleMemberKind::Function, n);
            Some(P::ModuleMember::Function(f))
        }
        P::ModuleMember::Constant(c) => {
            let n = c.name.0;
            check_name_and_add_implicit_alias!(ModuleMemberKind::Constant, n);
            Some(P::ModuleMember::Constant(c))
        }
        P::ModuleMember::Struct(s) => {
            let n = s.name.0;
            check_name_and_add_implicit_alias!(ModuleMemberKind::Struct, n);
            Some(P::ModuleMember::Struct(s))
        }
        P::ModuleMember::Spec(s) => {
            let sp!(
                _,
                SB {
                    target,
                    members,
                    ..
                }
            ) = &s;
            match &target.value {
                SBT::Schema(n, _) => {
                    check_name_and_add_implicit_alias!(ModuleMemberKind::Schema, *n);
                }
                SBT::Module => {
                    for sp!(_, smember_) in members {
                        if let SBM::Function { name, .. } = smember_ {
                            let n = name.0;
                            check_name_and_add_implicit_alias!(ModuleMemberKind::Function, n);
                        }
                    }
                }
                _ => (),
            };
            Some(P::ModuleMember::Spec(s))
        }
    }
}

fn uses(context: &mut Context, uses: Vec<P::UseDecl>) -> (AliasMapBuilder, UseFunsBuilder) {
    let mut new_scope = AliasMapBuilder::new();
    let mut use_funs = UseFunsBuilder::new();
    for u in uses {
        use_(context, &mut new_scope, &mut use_funs, u);
    }
    (new_scope, use_funs)
}

fn use_(
    context: &mut Context,
    acc: &mut AliasMapBuilder,
    use_funs: &mut UseFunsBuilder,
    u: P::UseDecl,
) {
    let P::UseDecl {
        use_: u,
        loc,
        attributes,
    } = u;
    let attributes = flatten_attributes(context, AttributePosition::Use, attributes);
    match u {
        P::Use::NestedModuleUses(address, use_decls) => {
            for (module, use_) in use_decls {
                let mident = sp(module.loc(), P::ModuleIdent_ { address, module });
                module_use(context, acc, use_funs, mident, &attributes, use_);
            }
        }
        P::Use::ModuleUse(mident, use_) => {
            module_use(context, acc, use_funs, mident, &attributes, use_);
        }
        P::Use::Fun {
            visibility,
            function,
            ty,
            method,
        } => {
            context
                .env
                .check_feature(FeatureGate::DotCall, context.current_package, loc);
            let is_public = match visibility {
                P::Visibility::Public(vis_loc) => Some(vis_loc),
                P::Visibility::Internal => None,
                P::Visibility::Script(vis_loc)
                | P::Visibility::Friend(vis_loc)
                | P::Visibility::Package(vis_loc) => {
                    let msg = "Invalid visibility for 'use fun' declaration";
                    let vis_msg = format!(
                        "Module level 'use fun' declarations can be '{}' for the module's types, \
                    otherwise they must internal to declared scope.",
                        P::Visibility::PUBLIC
                    );
                    context.env.add_diag(diag!(
                        Declarations::InvalidUseFun,
                        (loc, msg),
                        (vis_loc, vis_msg)
                    ));
                    None
                }
            };
            let explicit = ParserExplicitUseFun {
                loc,
                attributes,
                is_public,
                function,
                ty,
                method,
            };
            use_funs.explicit.push(explicit);
        }
    }
}

fn module_use(
    context: &mut Context,
    acc: &mut AliasMapBuilder,
    use_funs: &mut UseFunsBuilder,
    in_mident: P::ModuleIdent,
    attributes: &E::Attributes,
    muse: P::ModuleUse,
) {
    let unbound_module = |mident: &ModuleIdent| -> Diagnostic {
        diag!(
            NameResolution::UnboundModule,
            (
                mident.loc,
                format!("Invalid 'use'. Unbound module: '{}'", mident),
            )
        )
    };
    macro_rules! add_module_alias {
        ($ident:expr, $alias_opt:expr) => {{
            let alias: Name = $alias_opt.unwrap_or_else(|| $ident.value.module.0.clone());
            if let Err(()) = check_restricted_name_all_cases(context, NameCase::ModuleAlias, &alias)
            {
                return;
            }

            if let Err(old_loc) = acc.add_module_alias(alias.clone(), $ident) {
                duplicate_module_alias(context, old_loc, alias)
            }
        }};
    }
    match muse {
        P::ModuleUse::Module(alias_opt) => {
            let mident = module_ident(context, in_mident);
            if !context.module_members.contains_key(&mident) {
                context.env.add_diag(unbound_module(&mident));
                return;
            };
            add_module_alias!(mident, alias_opt.map(|m| m.0))
        }
        P::ModuleUse::Members(sub_uses) => {
            let mident = module_ident(context, in_mident);
            let members = match context.module_members.get(&mident) {
                Some(members) => members,
                None => {
                    context.env.add_diag(unbound_module(&mident));
                    return;
                }
            };
            let mloc = *context.module_members.get_loc(&mident).unwrap();
            let sub_uses_kinds = sub_uses
                .into_iter()
                .map(|(member, alia_opt)| {
                    let kind = members.get(&member).cloned();
                    (member, alia_opt, kind)
                })
                .collect::<Vec<_>>();

            for (member, alias_opt, member_kind_opt) in sub_uses_kinds {
                if member.value.as_str() == ModuleName::SELF_NAME {
                    add_module_alias!(mident, alias_opt);
                    continue;
                }

                // check is member

                let member_kind = match member_kind_opt {
                    None => {
                        let msg = format!(
                            "Invalid 'use'. Unbound member '{}' in module '{}'",
                            member, mident
                        );
                        context.env.add_diag(diag!(
                            NameResolution::UnboundModuleMember,
                            (member.loc, msg),
                            (mloc, format!("Module '{}' declared here", mident)),
                        ));
                        continue;
                    }
                    Some(m) => m,
                };

                let alias = alias_opt.unwrap_or(member);

                let alias = match check_valid_module_member_alias(context, member_kind, alias) {
                    None => continue,
                    Some(alias) => alias,
                };
                if let Err(old_loc) = acc.add_member_alias(alias, mident, member) {
                    duplicate_module_member(context, old_loc, alias)
                }
                if matches!(member_kind, ModuleMemberKind::Function) {
                    // remove any previously declared alias to keep in sync with the member alias
                    // map
                    use_funs.implicit.remove(&alias);
                    // not a function declaration
                    let is_public = None;
                    // assume used. We will set it to false if needed when exiting this alias scope
                    let kind = E::ImplicitUseFunKind::UseAlias { used: true };
                    let implicit = E::ImplicitUseFunCandidate {
                        loc: alias.loc,
                        attributes: attributes.clone(),
                        is_public,
                        function: (mident, member),
                        kind,
                    };
                    use_funs.implicit.add(alias, implicit).unwrap();
                }
            }
        }
    }
}

fn use_funs(context: &mut Context, builder: UseFunsBuilder) -> E::UseFuns {
    let UseFunsBuilder {
        explicit: pexplicit,
        implicit,
    } = builder;
    // If None, there was an error and we can skip it
    let explicit = pexplicit
        .into_iter()
        .filter_map(|e| explicit_use_fun(context, e))
        .collect();
    E::UseFuns { explicit, implicit }
}

fn explicit_use_fun(
    context: &mut Context,
    pexplicit: ParserExplicitUseFun,
) -> Option<E::ExplicitUseFun> {
    let ParserExplicitUseFun {
        loc,
        attributes,
        is_public,
        function,
        ty,
        method,
    } = pexplicit;
    let function = name_access_chain(context, Access::ApplyPositional, *function)?;
    let ty = name_access_chain(context, Access::Type, *ty)?;
    Some(E::ExplicitUseFun {
        loc,
        attributes,
        is_public,
        function,
        ty,
        method,
    })
}

fn duplicate_module_alias(context: &mut Context, old_loc: Loc, alias: Name) {
    let msg = format!(
        "Duplicate module alias '{}'. Module aliases must be unique within a given namespace",
        alias
    );
    context.env.add_diag(diag!(
        Declarations::DuplicateItem,
        (alias.loc, msg),
        (old_loc, "Alias previously defined here"),
    ));
}

fn duplicate_module_member(context: &mut Context, old_loc: Loc, alias: Name) {
    let msg = format!(
        "Duplicate module member or alias '{}'. Top level names in a namespace must be unique",
        alias
    );
    context.env.add_diag(diag!(
        Declarations::DuplicateItem,
        (alias.loc, msg),
        (old_loc, "Alias previously defined here"),
    ));
}

fn unused_alias(context: &mut Context, alias: Name) {
    if !context.is_source_definition {
        return;
    }

    context.env.add_diag(diag!(
        UnusedItem::Alias,
        (
            alias.loc,
            format!("Unused 'use' of alias '{}'. Consider removing it", alias)
        ),
    ))
}

//**************************************************************************************************
// Structs
//**************************************************************************************************

fn struct_def(
    context: &mut Context,
    structs: &mut UniqueMap<StructName, E::StructDefinition>,
    pstruct: P::StructDefinition,
) {
    let (sname, sdef) = struct_def_(context, structs.len(), pstruct);
    if let Err(_old_loc) = structs.add(sname, sdef) {
        assert!(context.env.has_errors())
    }
}

fn struct_def_(
    context: &mut Context,
    index: usize,
    pstruct: P::StructDefinition,
) -> (StructName, E::StructDefinition) {
    let P::StructDefinition {
        attributes,
        loc,
        name,
        abilities: abilities_vec,
        type_parameters: pty_params,
        fields: pfields,
    } = pstruct;
    let attributes = flatten_attributes(context, AttributePosition::Struct, attributes);
    let warning_filter = warning_filter(context, &attributes);
    context.env.add_warning_filter_scope(warning_filter.clone());
    let type_parameters = struct_type_parameters(context, pty_params);
    let old_aliases = context
        .aliases
        .shadow_for_type_parameters(type_parameters.iter().map(|tp| &tp.name));
    let abilities = ability_set(context, "modifier", abilities_vec);
    let fields = struct_fields(context, &name, pfields);
    let sdef = E::StructDefinition {
        warning_filter,
        index,
        attributes,
        loc,
        abilities,
        type_parameters,
        fields,
    };
    context.set_to_outer_scope(None, old_aliases);
    context.env.pop_warning_filter_scope();
    (name, sdef)
}

fn struct_fields(
    context: &mut Context,
    sname: &StructName,
    pfields: P::StructFields,
) -> E::StructFields {
    let pfields_vec = match pfields {
        P::StructFields::Native(loc) => return E::StructFields::Native(loc),
        P::StructFields::Positional(tys) => {
            let field_tys = tys.into_iter().map(|fty| type_(context, fty)).collect();
            return E::StructFields::Positional(field_tys);
        }
        P::StructFields::Defined(v) => v,
    };
    let mut field_map = UniqueMap::new();
    for (idx, (field, pt)) in pfields_vec.into_iter().enumerate() {
        let t = type_(context, pt);
        if let Err((field, old_loc)) = field_map.add(field, (idx, t)) {
            context.env.add_diag(diag!(
                Declarations::DuplicateItem,
                (
                    field.loc(),
                    format!(
                        "Duplicate definition for field '{}' in struct '{}'",
                        field, sname
                    ),
                ),
                (old_loc, "Field previously defined here"),
            ));
        }
    }
    E::StructFields::Named(field_map)
}

//**************************************************************************************************
// Friends
//**************************************************************************************************

fn friend(
    context: &mut Context,
    friends: &mut UniqueMap<ModuleIdent, E::Friend>,
    pfriend: P::FriendDecl,
) {
    match friend_(context, pfriend) {
        Some((mident, friend)) => match friends.get(&mident) {
            None => friends.add(mident, friend).unwrap(),
            Some(old_friend) => {
                let msg = format!(
                    "Duplicate friend declaration '{}'. Friend declarations in a module must be \
                     unique",
                    mident
                );
                context.env.add_diag(diag!(
                    Declarations::DuplicateItem,
                    (friend.loc, msg),
                    (old_friend.loc, "Friend previously declared here"),
                ));
            }
        },
        None => assert!(context.env.has_errors()),
    };
}

fn friend_(context: &mut Context, pfriend_decl: P::FriendDecl) -> Option<(ModuleIdent, E::Friend)> {
    assert!(context.exp_specs.is_empty());
    let P::FriendDecl {
        attributes: pattributes,
        loc,
        friend: pfriend,
    } = pfriend_decl;
    let mident = name_access_chain_to_module_ident(context, pfriend)?;
    let attributes = flatten_attributes(context, AttributePosition::Friend, pattributes);
    Some((mident, E::Friend { attributes, loc }))
}

//**************************************************************************************************
// Constants
//**************************************************************************************************

fn constant(
    context: &mut Context,
    constants: &mut UniqueMap<ConstantName, E::Constant>,
    pconstant: P::Constant,
) {
    let (name, constant) = constant_(context, constants.len(), pconstant);
    if let Err(_old_loc) = constants.add(name, constant) {
        assert!(context.env.has_errors())
    }
}

fn constant_(
    context: &mut Context,
    index: usize,
    pconstant: P::Constant,
) -> (ConstantName, E::Constant) {
    assert!(context.exp_specs.is_empty());
    let P::Constant {
        attributes: pattributes,
        loc,
        name,
        signature: psignature,
        value: pvalue,
    } = pconstant;
    let attributes = flatten_attributes(context, AttributePosition::Constant, pattributes);
    let warning_filter = warning_filter(context, &attributes);
    context.env.add_warning_filter_scope(warning_filter.clone());
    let signature = type_(context, psignature);
    let value = exp_(context, pvalue);
    let _specs = context.extract_exp_specs();
    let constant = E::Constant {
        warning_filter,
        index,
        attributes,
        loc,
        signature,
        value,
    };
    context.env.pop_warning_filter_scope();
    (name, constant)
}

//**************************************************************************************************
// Functions
//**************************************************************************************************

fn function(
    context: &mut Context,
    module_and_use_funs: Option<(ModuleIdent, &mut UseFunsBuilder)>,
    functions: &mut UniqueMap<FunctionName, E::Function>,
    pfunction: P::Function,
) {
    let (fname, fdef) = function_(context, module_and_use_funs, functions.len(), pfunction);
    if let Err(_old_loc) = functions.add(fname, fdef) {
        assert!(context.env.has_errors())
    }
}

fn function_(
    context: &mut Context,
    module_and_use_funs: Option<(ModuleIdent, &mut UseFunsBuilder)>,
    index: usize,
    pfunction: P::Function,
) -> (FunctionName, E::Function) {
    let P::Function {
        attributes: pattributes,
        loc,
        name,
        visibility: pvisibility,
        entry,
        signature: psignature,
        body: pbody,
    } = pfunction;
    assert!(context.exp_specs.is_empty());
    let attributes = flatten_attributes(context, AttributePosition::Function, pattributes);
    let warning_filter = warning_filter(context, &attributes);
    context.env.add_warning_filter_scope(warning_filter.clone());
    let visibility = visibility(pvisibility);
    let (old_aliases, signature) = function_signature(context, psignature);
    let body = function_body(context, pbody);
    let specs = context.extract_exp_specs();
    if let Some((m, use_funs_builder)) = module_and_use_funs {
        let implicit = E::ImplicitUseFunCandidate {
            loc: name.loc(),
            attributes: attributes.clone(),
            is_public: Some(visibility.loc().unwrap_or_else(|| name.loc())),
            function: (m, name.0),
            // disregard used/unused information tracking
            kind: E::ImplicitUseFunKind::FunctionDeclaration,
        };
        // we can ignore any error, since the alias map will catch conflicting names
        let _ = use_funs_builder.implicit.add(name.0, implicit);
    }
    let fdef = E::Function {
        warning_filter,
        index,
        attributes,
        loc,
        visibility,
        entry,
        signature,
        body,
        specs,
    };
    context.set_to_outer_scope(None, old_aliases);
    context.env.pop_warning_filter_scope();
    (name, fdef)
}

fn visibility(pvisibility: P::Visibility) -> E::Visibility {
    match pvisibility {
        P::Visibility::Friend(loc) => E::Visibility::Friend(loc),
        P::Visibility::Internal => E::Visibility::Internal,
        P::Visibility::Package(loc) => E::Visibility::Package(loc),
        P::Visibility::Public(loc) => E::Visibility::Public(loc),
        P::Visibility::Script(loc) => E::Visibility::Public(loc),
    }
}

fn function_signature(
    context: &mut Context,
    psignature: P::FunctionSignature,
) -> (OldAliasMap, E::FunctionSignature) {
    let P::FunctionSignature {
        type_parameters: pty_params,
        parameters: pparams,
        return_type: pret_ty,
    } = psignature;
    let type_parameters = type_parameters(context, pty_params);
    let old_aliases = context
        .aliases
        .shadow_for_type_parameters(type_parameters.iter().map(|(name, _)| name));
    let parameters = pparams
        .into_iter()
        .map(|(pmut, v, t)| (mutability(context, v.loc(), pmut), v, type_(context, t)))
        .collect::<Vec<_>>();
    for (_, v, _) in &parameters {
        check_valid_local_name(context, v)
    }
    let return_type = type_(context, pret_ty);
    let signature = E::FunctionSignature {
        type_parameters,
        parameters,
        return_type,
    };
    (old_aliases, signature)
}

fn function_body(context: &mut Context, sp!(loc, pbody_): P::FunctionBody) -> E::FunctionBody {
    use E::FunctionBody_ as EF;
    use P::FunctionBody_ as PF;
    let body_ = match pbody_ {
        PF::Native => EF::Native,
        PF::Defined(seq) => EF::Defined(sequence(context, loc, seq)),
    };
    sp(loc, body_)
}

//**************************************************************************************************
// Specification Blocks
//**************************************************************************************************

fn specs(context: &mut Context, pspecs: Vec<P::SpecBlock>) -> Vec<E::SpecBlock> {
    pspecs.into_iter().map(|s| spec(context, s)).collect()
}

fn spec(context: &mut Context, sp!(loc, pspec): P::SpecBlock) -> E::SpecBlock {
    let P::SpecBlock_ {
        attributes: pattributes,
        target,
        uses: puses,
        members: pmembers,
    } = pspec;

    let attributes = flatten_attributes(context, AttributePosition::Spec, pattributes);
    context.in_spec_context = true;
    let (new_scope, use_funs_builder) = uses(context, puses);
    // Use funs not supported in specs
    for use_fun in use_funs_builder.explicit {
        let msg = "'use fun' declarations are not supported in spec blocks";
        context
            .env
            .add_diag(diag!(Declarations::InvalidUseFun, (use_fun.loc, msg)))
    }
    let old_aliases = context.aliases.add_and_shadow_all(new_scope);

    let members = pmembers
        .into_iter()
        .map(|m| spec_member(context, m))
        .collect();

    context.set_to_outer_scope(None, old_aliases);
    context.in_spec_context = false;

    sp(
        loc,
        E::SpecBlock_ {
            attributes,
            target: spec_target(context, target),
            members,
        },
    )
}

fn spec_target(context: &mut Context, sp!(loc, pt): P::SpecBlockTarget) -> E::SpecBlockTarget {
    use E::SpecBlockTarget_ as ET;
    use P::SpecBlockTarget_ as PT;
    let et = match pt {
        PT::Code => ET::Code,
        PT::Module => ET::Module,
        PT::Schema(name, type_params) => ET::Schema(name, type_parameters(context, type_params)),
        PT::Member(name, signature_opt) => ET::Member(
            name,
            signature_opt.map(|s| {
                let (old_aliases, signature) = function_signature(context, *s);
                context.set_to_outer_scope(None, old_aliases);
                Box::new(signature)
            }),
        ),
    };
    sp(loc, et)
}

fn spec_condition_kind(
    context: &mut Context,
    sp!(loc, kind): P::SpecConditionKind,
) -> (E::SpecConditionKind, Option<OldAliasMap>) {
    let (kind_, aliases_opt) = match kind {
        P::SpecConditionKind_::Assert => (E::SpecConditionKind_::Assert, None),
        P::SpecConditionKind_::Assume => (E::SpecConditionKind_::Assume, None),
        P::SpecConditionKind_::Decreases => (E::SpecConditionKind_::Decreases, None),
        P::SpecConditionKind_::AbortsIf => (E::SpecConditionKind_::AbortsIf, None),
        P::SpecConditionKind_::AbortsWith => (E::SpecConditionKind_::AbortsWith, None),
        P::SpecConditionKind_::SucceedsIf => (E::SpecConditionKind_::SucceedsIf, None),
        P::SpecConditionKind_::Modifies => (E::SpecConditionKind_::Modifies, None),
        P::SpecConditionKind_::Emits => (E::SpecConditionKind_::Emits, None),
        P::SpecConditionKind_::Ensures => (E::SpecConditionKind_::Ensures, None),
        P::SpecConditionKind_::Requires => (E::SpecConditionKind_::Requires, None),
        P::SpecConditionKind_::Invariant(pty_params) => {
            let ety_params = type_parameters(context, pty_params);
            let old_aliases = context
                .aliases
                .shadow_for_type_parameters(ety_params.iter().map(|(name, _)| name));
            (
                E::SpecConditionKind_::Invariant(ety_params),
                Some(old_aliases),
            )
        }
        P::SpecConditionKind_::InvariantUpdate(pty_params) => {
            let ety_params = type_parameters(context, pty_params);
            let old_aliases = context
                .aliases
                .shadow_for_type_parameters(ety_params.iter().map(|(name, _)| name));
            (
                E::SpecConditionKind_::InvariantUpdate(ety_params),
                Some(old_aliases),
            )
        }
        P::SpecConditionKind_::Axiom(pty_params) => {
            let ety_params = type_parameters(context, pty_params);
            let old_aliases = context
                .aliases
                .shadow_for_type_parameters(ety_params.iter().map(|(name, _)| name));
            (E::SpecConditionKind_::Axiom(ety_params), Some(old_aliases))
        }
    };
    (sp(loc, kind_), aliases_opt)
}

fn spec_member(context: &mut Context, sp!(loc, pm): P::SpecBlockMember) -> E::SpecBlockMember {
    use E::SpecBlockMember_ as EM;
    use P::SpecBlockMember_ as PM;
    let em = match pm {
        PM::Condition {
            kind: pkind,
            properties: pproperties,
            exp,
            additional_exps,
        } => {
            let (kind, old_aliases_opt) = spec_condition_kind(context, pkind);
            let properties = pproperties
                .into_iter()
                .map(|p| pragma_property(context, p))
                .collect();
            let exp = exp_(context, exp);
            let additional_exps = additional_exps
                .into_iter()
                .map(|e| exp_(context, e))
                .collect();
            match old_aliases_opt {
                None => (),
                Some(old_aliases) => context.set_to_outer_scope(None, old_aliases),
            }
            EM::Condition {
                kind,
                properties,
                exp,
                additional_exps,
            }
        }
        PM::Function {
            name,
            uninterpreted,
            signature,
            body,
        } => {
            let (old_aliases, signature) = function_signature(context, signature);
            let body = function_body(context, body);
            context.set_to_outer_scope(None, old_aliases);
            EM::Function {
                uninterpreted,
                name,
                signature,
                body,
            }
        }
        PM::Variable {
            is_global,
            name,
            type_parameters: pty_params,
            type_: t,
            init,
        } => {
            let type_parameters = type_parameters(context, pty_params);
            let old_aliases = context
                .aliases
                .shadow_for_type_parameters(type_parameters.iter().map(|(name, _)| name));
            let t = type_(context, t);
            let i = init.map(|e| exp_(context, e));
            context.set_to_outer_scope(None, old_aliases);
            EM::Variable {
                is_global,
                name,
                type_parameters,
                type_: t,
                init: i,
            }
        }
        PM::Update { lhs, rhs } => {
            let lhs = exp_(context, lhs);
            let rhs = exp_(context, rhs);
            EM::Update { lhs, rhs }
        }

        PM::Let {
            name,
            post_state: old,
            def: pdef,
        } => {
            let def = exp_(context, pdef);
            EM::Let {
                name,
                post_state: old,
                def,
            }
        }
        PM::Include {
            properties: pproperties,
            exp: pexp,
        } => {
            let properties = pproperties
                .into_iter()
                .map(|p| pragma_property(context, p))
                .collect();
            EM::Include {
                properties,
                exp: exp_(context, pexp),
            }
        }
        PM::Apply {
            exp: pexp,
            patterns,
            exclusion_patterns,
        } => EM::Apply {
            exp: exp_(context, pexp),
            patterns,
            exclusion_patterns,
        },
        PM::Pragma {
            properties: pproperties,
        } => {
            let properties = pproperties
                .into_iter()
                .map(|p| pragma_property(context, p))
                .collect();
            EM::Pragma { properties }
        }
    };
    sp(loc, em)
}

fn pragma_property(context: &mut Context, sp!(loc, pp_): P::PragmaProperty) -> E::PragmaProperty {
    let P::PragmaProperty_ {
        name,
        value: pv_opt,
    } = pp_;
    let value = pv_opt.and_then(|pv| pragma_value(context, pv));
    sp(loc, E::PragmaProperty_ { name, value })
}

fn pragma_value(context: &mut Context, pv: P::PragmaValue) -> Option<E::PragmaValue> {
    match pv {
        P::PragmaValue::Literal(v) => value(context, v).map(E::PragmaValue::Literal),
        P::PragmaValue::Ident(ma) => {
            name_access_chain(context, Access::Term, ma).map(E::PragmaValue::Ident)
        }
    }
}

//**************************************************************************************************
// Types
//**************************************************************************************************

fn ability_set(context: &mut Context, case: &str, abilities_vec: Vec<Ability>) -> E::AbilitySet {
    let mut set = E::AbilitySet::empty();
    for ability in abilities_vec {
        let loc = ability.loc;
        if let Err(prev_loc) = set.add(ability) {
            context.env.add_diag(diag!(
                Declarations::DuplicateItem,
                (loc, format!("Duplicate '{}' ability {}", ability, case)),
                (prev_loc, "Ability previously given here")
            ));
        }
    }
    set
}

fn type_parameters(
    context: &mut Context,
    pty_params: Vec<(Name, Vec<Ability>)>,
) -> Vec<(Name, E::AbilitySet)> {
    pty_params
        .into_iter()
        .map(|(name, constraints_vec)| {
            let constraints = ability_set(context, "constraint", constraints_vec);
            (name, constraints)
        })
        .collect()
}

fn struct_type_parameters(
    context: &mut Context,
    pty_params: Vec<P::StructTypeParameter>,
) -> Vec<E::StructTypeParameter> {
    pty_params
        .into_iter()
        .map(|param| E::StructTypeParameter {
            is_phantom: param.is_phantom,
            name: param.name,
            constraints: ability_set(context, "constraint", param.constraints),
        })
        .collect()
}

fn type_(context: &mut Context, sp!(loc, pt_): P::Type) -> E::Type {
    use E::Type_ as ET;
    use P::Type_ as PT;
    let t_ = match pt_ {
        PT::Unit => ET::Unit,
        PT::Multiple(ts) => ET::Multiple(types(context, ts)),
        PT::Apply(pn, ptyargs) => {
            let tyargs = types(context, ptyargs);
            match name_access_chain(context, Access::Type, *pn) {
                None => {
                    assert!(context.env.has_errors());
                    ET::UnresolvedError
                }
                Some(n) => ET::Apply(n, tyargs),
            }
        }
        PT::Ref(mut_, inner) => ET::Ref(mut_, Box::new(type_(context, *inner))),
        PT::Fun(args, result) => {
            if context.in_spec_context {
                let args = types(context, args);
                let result = type_(context, *result);
                ET::Fun(args, Box::new(result))
            } else {
                context.env.add_diag(diag!(
                    Syntax::SpecContextRestricted,
                    (loc, "`|_|_` function type only allowed in specifications")
                ));
                ET::UnresolvedError
            }
        }
    };
    sp(loc, t_)
}

fn types(context: &mut Context, pts: Vec<P::Type>) -> Vec<E::Type> {
    pts.into_iter().map(|pt| type_(context, pt)).collect()
}

fn optional_types(context: &mut Context, pts_opt: Option<Vec<P::Type>>) -> Option<Vec<E::Type>> {
    pts_opt.map(|pts| pts.into_iter().map(|pt| type_(context, pt)).collect())
}

#[derive(Clone, Copy)]
enum Access {
    Type,
    ApplyNamed,
    ApplyPositional,
    Term,
}

fn name_access_chain(
    context: &mut Context,
    access: Access,
    sp!(loc, ptn_): P::NameAccessChain,
) -> Option<E::ModuleAccess> {
    use E::ModuleAccess_ as EN;
    use P::{LeadingNameAccess_ as LN, NameAccessChain_ as PN};

    let tn_ = match (access, ptn_) {
        (Access::ApplyPositional, PN::One(n))
        | (Access::ApplyNamed, PN::One(n))
        | (Access::Type, PN::One(n)) => match context.aliases.member_alias_get(&n) {
            Some((mident, mem)) => EN::ModuleAccess(mident, mem),
            None => EN::Name(n),
        },
        (Access::Term, PN::One(n)) if is_valid_struct_constant_or_schema_name(n.value.as_str()) => {
            match context.aliases.member_alias_get(&n) {
                Some((mident, mem)) => EN::ModuleAccess(mident, mem),
                None => EN::Name(n),
            }
        }
        (Access::Term, PN::One(n)) => EN::Name(n),
        (_, PN::Two(sp!(nloc, LN::AnonymousAddress(_)), _)) => {
            context
                .env
                .add_diag(unexpected_address_module_error(loc, nloc, access));
            return None;
        }

        (_, PN::Two(sp!(_, LN::Name(n1)), n2)) => match context.aliases.module_alias_get(&n1) {
            None => {
                context.env.add_diag(diag!(
                    NameResolution::UnboundModule,
                    (n1.loc, format!("Unbound module alias '{}'", n1))
                ));
                return None;
            }
            Some(mident) => EN::ModuleAccess(mident, n2),
        },
        (_, PN::Three(sp!(ident_loc, (ln, n2)), n3)) => {
            let addr = address(context, /* suggest_declaration */ false, ln);
            let mident = sp(ident_loc, ModuleIdent_::new(addr, ModuleName(n2)));
            EN::ModuleAccess(mident, n3)
        }
    };
    Some(sp(loc, tn_))
}

fn name_access_chain_to_module_ident(
    context: &mut Context,
    sp!(loc, pn_): P::NameAccessChain,
) -> Option<E::ModuleIdent> {
    use P::NameAccessChain_ as PN;
    match pn_ {
        PN::One(name) => match context.aliases.module_alias_get(&name) {
            None => {
                context.env.add_diag(diag!(
                    NameResolution::UnboundModule,
                    (name.loc, format!("Unbound module alias '{}'", name)),
                ));
                None
            }
            Some(mident) => Some(mident),
        },
        PN::Two(ln, n) => {
            let pmident_ = P::ModuleIdent_ {
                address: ln,
                module: ModuleName(n),
            };
            Some(module_ident(context, sp(loc, pmident_)))
        }
        PN::Three(sp!(ident_loc, (ln, n)), mem) => {
            // Process the module ident just for errors
            let pmident_ = P::ModuleIdent_ {
                address: ln,
                module: ModuleName(n),
            };
            let _ = module_ident(context, sp(ident_loc, pmident_));
            context.env.add_diag(diag!(
                NameResolution::NamePositionMismatch,
                (
                    mem.loc,
                    "Unexpected module member access. Expected a module identifier only",
                )
            ));
            None
        }
    }
}

fn unexpected_address_module_error(loc: Loc, nloc: Loc, access: Access) -> Diagnostic {
    let case = match access {
        Access::Type | Access::ApplyNamed | Access::ApplyPositional => "type",
        Access::Term => "expression",
    };
    let unexpected_msg = format!(
        "Unexpected module identifier. A module identifier is not a valid {}",
        case
    );
    diag!(
        NameResolution::NamePositionMismatch,
        (loc, unexpected_msg),
        (nloc, "Expected a module name".to_owned()),
    )
}

//**************************************************************************************************
// Expressions
//**************************************************************************************************

fn sequence(context: &mut Context, loc: Loc, seq: P::Sequence) -> E::Sequence {
    let (puses, pitems, maybe_last_semicolon_loc, pfinal_item) = seq;

    let (new_scope, use_funs_builder) = uses(context, puses);
    let old_aliases = context.aliases.add_and_shadow_all(new_scope);
    let mut use_funs = use_funs(context, use_funs_builder);
    let mut items: VecDeque<E::SequenceItem> = pitems
        .into_iter()
        .map(|item| sequence_item(context, item))
        .collect();
    let final_e_opt = pfinal_item.map(|item| exp_(context, item));
    let final_e = match final_e_opt {
        None => {
            let last_semicolon_loc = match maybe_last_semicolon_loc {
                Some(l) => l,
                None => loc,
            };
            sp(last_semicolon_loc, E::Exp_::Unit { trailing: true })
        }
        Some(e) => e,
    };
    let final_item = sp(final_e.loc, E::SequenceItem_::Seq(final_e));
    items.push_back(final_item);
    context.set_to_outer_scope(Some(&mut use_funs), old_aliases);
    (use_funs, items)
}

fn sequence_item(context: &mut Context, sp!(loc, pitem_): P::SequenceItem) -> E::SequenceItem {
    use E::SequenceItem_ as ES;
    use P::SequenceItem_ as PS;
    let item_ = match pitem_ {
        PS::Seq(e) => ES::Seq(exp_(context, *e)),
        PS::Declare(pb, pty_opt) => {
            let b_opt = bind_list(context, pb);
            let ty_opt = pty_opt.map(|t| type_(context, t));
            match b_opt {
                None => {
                    assert!(context.env.has_errors());
                    ES::Seq(sp(loc, E::Exp_::UnresolvedError))
                }
                Some(b) => ES::Declare(b, ty_opt),
            }
        }
        PS::Bind(pb, pty_opt, pe) => {
            let b_opt = bind_list(context, pb);
            let ty_opt = pty_opt.map(|t| type_(context, t));
            let e_ = exp_(context, *pe);
            let e = match ty_opt {
                None => e_,
                Some(ty) => sp(e_.loc, E::Exp_::Annotate(Box::new(e_), ty)),
            };
            match b_opt {
                None => {
                    assert!(context.env.has_errors());
                    ES::Seq(sp(loc, E::Exp_::UnresolvedError))
                }
                Some(b) => ES::Bind(b, e),
            }
        }
    };
    sp(loc, item_)
}

fn exps(context: &mut Context, pes: Vec<P::Exp>) -> Vec<E::Exp> {
    pes.into_iter().map(|pe| exp_(context, pe)).collect()
}

fn exp(context: &mut Context, pe: P::Exp) -> Box<E::Exp> {
    Box::new(exp_(context, pe))
}

fn exp_(context: &mut Context, sp!(loc, pe_): P::Exp) -> E::Exp {
    use E::Exp_ as EE;
    use P::Exp_ as PE;
    let e_ = match pe_ {
        PE::Unit => EE::Unit { trailing: false },
        PE::Value(pv) => match value(context, pv) {
            Some(v) => EE::Value(v),
            None => {
                assert!(context.env.has_errors());
                EE::UnresolvedError
            }
        },
        PE::Move(v) => EE::Move(v),
        PE::Copy(v) => EE::Copy(v),
        PE::Name(_, Some(_)) if !context.in_spec_context => {
            context.env.add_diag(diag!(
                Syntax::SpecContextRestricted,
                (
                    loc,
                    "Expected name to be followed by a brace-enclosed list of field expressions \
                     or a parenthesized list of arguments for a function call",
                )
            ));
            EE::UnresolvedError
        }
        PE::Name(pn, ptys_opt) => {
            let en_opt = name_access_chain(context, Access::Term, pn);
            let tys_opt = optional_types(context, ptys_opt);
            match en_opt {
                Some(en) => EE::Name(en, tys_opt),
                None => {
                    assert!(context.env.has_errors());
                    EE::UnresolvedError
                }
            }
        }
        PE::Call(pn, is_macro, ptys_opt, sp!(rloc, prs)) => {
            let tys_opt = optional_types(context, ptys_opt);
            let ers = sp(rloc, exps(context, prs));
            let en_opt = name_access_chain(context, Access::ApplyPositional, pn);
            match en_opt {
                Some(en) => EE::Call(en, is_macro, tys_opt, ers),
                None => {
                    assert!(context.env.has_errors());
                    EE::UnresolvedError
                }
            }
        }
        PE::Pack(pn, ptys_opt, pfields) => {
            let en_opt = name_access_chain(context, Access::ApplyNamed, pn);
            let tys_opt = optional_types(context, ptys_opt);
            let efields_vec = pfields
                .into_iter()
                .map(|(f, pe)| (f, exp_(context, pe)))
                .collect();
            let efields = named_fields(context, loc, "construction", "argument", efields_vec);
            match en_opt {
                Some(en) => EE::Pack(en, tys_opt, efields),
                None => {
                    assert!(context.env.has_errors());
                    EE::UnresolvedError
                }
            }
        }
        PE::Vector(vec_loc, ptys_opt, sp!(args_loc, pargs_)) => {
            let tys_opt = optional_types(context, ptys_opt);
            let args = sp(args_loc, exps(context, pargs_));
            EE::Vector(vec_loc, tys_opt, args)
        }
        PE::IfElse(pb, pt, pf_opt) => {
            let eb = exp(context, *pb);
            let et = exp(context, *pt);
            let ef = match pf_opt {
                None => Box::new(sp(loc, EE::Unit { trailing: false })),
                Some(pf) => exp(context, *pf),
            };
            EE::IfElse(eb, et, ef)
        }
        PE::While(pb, ploop) => EE::While(exp(context, *pb), exp(context, *ploop)),
        PE::Loop(ploop) => EE::Loop(exp(context, *ploop)),
        PE::Block(seq) => EE::Block(sequence(context, loc, seq)),
        PE::Lambda(pbs, pe) => {
            if !context.in_spec_context {
                context.env.add_diag(diag!(
                    Syntax::SpecContextRestricted,
                    (loc, "lambda expression only allowed in specifications"),
                ));
                EE::UnresolvedError
            } else {
                let bs_opt = bind_list(context, pbs);
                let e = exp_(context, *pe);
                match bs_opt {
                    Some(bs) => EE::Lambda(bs, Box::new(e)),
                    None => {
                        assert!(context.env.has_errors());
                        EE::UnresolvedError
                    }
                }
            }
        }
        PE::Quant(k, prs, ptrs, pc, pe) => {
            if !context.in_spec_context {
                context.env.add_diag(diag!(
                    Syntax::SpecContextRestricted,
                    (loc, "quantifer expression only allowed in specifications")
                ));
                EE::UnresolvedError
            } else {
                let rs_opt = bind_with_range_list(context, prs);
                let rtrs = ptrs
                    .into_iter()
                    .map(|trs| trs.into_iter().map(|tr| exp_(context, tr)).collect())
                    .collect();
                let rc = pc.map(|c| Box::new(exp_(context, *c)));
                let re = exp_(context, *pe);
                match rs_opt {
                    Some(rs) => EE::Quant(k, rs, rtrs, rc, Box::new(re)),
                    None => {
                        assert!(context.env.has_errors());
                        EE::UnresolvedError
                    }
                }
            }
        }
        PE::ExpList(pes) => {
            assert!(pes.len() > 1);
            EE::ExpList(exps(context, pes))
        }

        PE::Assign(lvalue, rhs) => {
            let l_opt = lvalues(context, *lvalue);
            let er = exp(context, *rhs);
            match l_opt {
                None => {
                    assert!(context.env.has_errors());
                    EE::UnresolvedError
                }
                Some(LValue::Assigns(al)) => EE::Assign(al, er),
                Some(LValue::Mutate(el)) => EE::Mutate(el, er),
                Some(LValue::FieldMutate(edotted)) => EE::FieldMutate(edotted, er),
            }
        }
        PE::Return(pe_opt) => {
            let ev = match pe_opt {
                None => Box::new(sp(loc, EE::Unit { trailing: false })),
                Some(pe) => exp(context, *pe),
            };
            EE::Return(ev)
        }
        PE::Abort(pe) => EE::Abort(exp(context, *pe)),
        PE::Break => EE::Break,
        PE::Continue => EE::Continue,
        PE::Dereference(pe) => EE::Dereference(exp(context, *pe)),
        PE::UnaryExp(op, pe) => EE::UnaryExp(op, exp(context, *pe)),
        PE::BinopExp(pl, op, pr) => {
            if op.value.is_spec_only() && !context.in_spec_context {
                let msg = format!(
                    "`{}` operator only allowed in specifications",
                    op.value.symbol()
                );
                context
                    .env
                    .add_diag(diag!(Syntax::SpecContextRestricted, (loc, msg)));
                EE::UnresolvedError
            } else {
                EE::BinopExp(exp(context, *pl), op, exp(context, *pr))
            }
        }
        PE::Borrow(mut_, pr) => EE::Borrow(mut_, exp(context, *pr)),
        pdotted_ @ PE::Dot(_, _) => match exp_dotted(context, sp(loc, pdotted_)) {
            Some(edotted) => EE::ExpDotted(Box::new(edotted)),
            None => {
                assert!(context.env.has_errors());
                EE::UnresolvedError
            }
        },
        PE::DotCall(pdotted, n, ptys_opt, sp!(rloc, prs)) => match exp_dotted(context, *pdotted) {
            Some(edotted) => {
                context
                    .env
                    .check_feature(FeatureGate::DotCall, context.current_package, loc);
                if context.in_spec_context {
                    let msg = "method syntax is not supported in specifications";
                    context
                        .env
                        .add_diag(diag!(Syntax::SpecContextRestricted, (loc, msg)));
                    EE::UnresolvedError
                } else {
                    let tys_opt = optional_types(context, ptys_opt);
                    let ers = sp(rloc, exps(context, prs));
                    EE::MethodCall(Box::new(edotted), n, tys_opt, ers)
                }
            }
            None => {
                assert!(context.env.has_errors());
                EE::UnresolvedError
            }
        },
        PE::Cast(e, ty) => EE::Cast(exp(context, *e), type_(context, ty)),
        PE::Index(e, i) => {
            if context.in_spec_context {
                EE::Index(exp(context, *e), exp(context, *i))
            } else {
                let msg = "`_[_]` index operator only allowed in specifications";
                context
                    .env
                    .add_diag(diag!(Syntax::SpecContextRestricted, (loc, msg)));
                EE::UnresolvedError
            }
        }
        PE::Annotate(e, ty) => EE::Annotate(exp(context, *e), type_(context, ty)),
        PE::Spec(_) if context.in_spec_context => {
            context.env.add_diag(diag!(
                Syntax::SpecContextRestricted,
                (loc, "'spec' blocks cannot be used inside of a spec context",)
            ));
            EE::UnresolvedError
        }
        PE::Spec(spec_block) => {
            let (spec_id, unbound_names) = context.bind_exp_spec(spec_block);
            EE::Spec(spec_id, unbound_names)
        }
        PE::UnresolvedError => panic!("ICE error should have been thrown"),
    };
    sp(loc, e_)
}

fn exp_dotted(context: &mut Context, sp!(loc, pdotted_): P::Exp) -> Option<E::ExpDotted> {
    use E::ExpDotted_ as EE;
    use P::Exp_ as PE;
    let edotted_ = match pdotted_ {
        PE::Dot(plhs, field) => {
            let lhs = exp_dotted(context, *plhs)?;
            EE::Dot(Box::new(lhs), field)
        }
        pe_ => EE::Exp(Box::new(exp_(context, sp(loc, pe_)))),
    };
    Some(sp(loc, edotted_))
}

fn value(context: &mut Context, sp!(loc, pvalue_): P::Value) -> Option<E::Value> {
    use E::Value_ as EV;
    use P::Value_ as PV;
    let value_ = match pvalue_ {
        PV::Address(addr) => {
            let addr = address(context, /* suggest_declaration */ true, addr);
            EV::Address(addr)
        }
        PV::Num(s) if s.ends_with("u8") => match parse_u8(&s[..s.len() - 2]) {
            Ok((u, _format)) => EV::U8(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(loc, "'u8'"));
                return None;
            }
        },
        PV::Num(s) if s.ends_with("u16") => match parse_u16(&s[..s.len() - 3]) {
            Ok((u, _format)) => EV::U16(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(loc, "'u16'"));
                return None;
            }
        },
        PV::Num(s) if s.ends_with("u32") => match parse_u32(&s[..s.len() - 3]) {
            Ok((u, _format)) => EV::U32(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(loc, "'u32'"));
                return None;
            }
        },
        PV::Num(s) if s.ends_with("u64") => match parse_u64(&s[..s.len() - 3]) {
            Ok((u, _format)) => EV::U64(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(loc, "'u64'"));
                return None;
            }
        },
        PV::Num(s) if s.ends_with("u128") => match parse_u128(&s[..s.len() - 4]) {
            Ok((u, _format)) => EV::U128(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(loc, "'u128'"));
                return None;
            }
        },
        PV::Num(s) if s.ends_with("u256") => match parse_u256(&s[..s.len() - 4]) {
            Ok((u, _format)) => EV::U256(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(loc, "'u256'"));
                return None;
            }
        },

        PV::Num(s) => match parse_u256(&s) {
            Ok((u, _format)) => EV::InferredNum(u),
            Err(_) => {
                context.env.add_diag(num_too_big_error(
                    loc,
                    "the largest possible integer type, 'u256'",
                ));
                return None;
            }
        },
        PV::Bool(b) => EV::Bool(b),
        PV::HexString(s) => match hex_string::decode(loc, &s) {
            Ok(v) => EV::Bytearray(v),
            Err(e) => {
                context.env.add_diag(*e);
                return None;
            }
        },
        PV::ByteString(s) => match byte_string::decode(loc, &s) {
            Ok(v) => EV::Bytearray(v),
            Err(e) => {
                context.env.add_diags(e);
                return None;
            }
        },
    };
    Some(sp(loc, value_))
}

// Create an error for an integer literal that is too big to fit in its type.
// This assumes that the literal is the current token.
fn num_too_big_error(loc: Loc, type_description: &'static str) -> Diagnostic {
    diag!(
        Syntax::InvalidNumber,
        (
            loc,
            format!(
                "Invalid number literal. The given literal is too large to fit into {}",
                type_description
            )
        ),
    )
}

//**************************************************************************************************
// Fields
//**************************************************************************************************

fn named_fields<T>(
    context: &mut Context,
    loc: Loc,
    case: &str,
    verb: &str,
    xs: Vec<(Field, T)>,
) -> Fields<T> {
    let mut fmap = UniqueMap::new();
    for (idx, (field, x)) in xs.into_iter().enumerate() {
        if let Err((field, old_loc)) = fmap.add(field, (idx, x)) {
            context.env.add_diag(diag!(
                Declarations::DuplicateItem,
                (loc, format!("Invalid {}", case)),
                (
                    field.loc(),
                    format!("Duplicate {} given for field '{}'", verb, field),
                ),
                (old_loc, "Field previously defined here".into()),
            ))
        }
    }
    fmap
}

//**************************************************************************************************
// LValues
//**************************************************************************************************

fn bind_list(context: &mut Context, sp!(loc, pbs_): P::BindList) -> Option<E::LValueList> {
    let bs_: Option<Vec<E::LValue>> = pbs_.into_iter().map(|pb| bind(context, pb)).collect();
    Some(sp(loc, bs_?))
}

fn bind_with_range_list(
    context: &mut Context,
    sp!(loc, prs_): P::BindWithRangeList,
) -> Option<E::LValueWithRangeList> {
    let rs_: Option<Vec<E::LValueWithRange>> = prs_
        .into_iter()
        .map(|sp!(loc, (pb, pr))| -> Option<E::LValueWithRange> {
            let r = exp_(context, pr);
            let b = bind(context, pb)?;
            Some(sp(loc, (b, r)))
        })
        .collect();
    Some(sp(loc, rs_?))
}

fn bind(context: &mut Context, sp!(loc, pb_): P::Bind) -> Option<E::LValue> {
    use E::LValue_ as EL;
    use P::Bind_ as PB;
    let b_ = match pb_ {
        PB::Var(pmut, v) => {
            let emut = mutability(context, v.loc(), pmut);
            check_valid_local_name(context, &v);
            EL::Var(emut, sp(loc, E::ModuleAccess_::Name(v.0)), None)
        }
        PB::Unpack(ptn, ptys_opt, pfields) => {
            let tn = name_access_chain(context, Access::ApplyNamed, *ptn)?;
            let tys_opt = optional_types(context, ptys_opt);
            let fields = match pfields {
                FieldBindings::Named(named_bindings) => {
                    let vfields: Option<Vec<(Field, E::LValue)>> = named_bindings
                        .into_iter()
                        .map(|(f, pb)| Some((f, bind(context, pb)?)))
                        .collect();
                    let fields =
                        named_fields(context, loc, "deconstruction binding", "binding", vfields?);
                    E::FieldBindings::Named(fields)
                }
                FieldBindings::Positional(positional_bindings) => {
                    let fields: Option<Vec<E::LValue>> = positional_bindings
                        .into_iter()
                        .map(|b| bind(context, b))
                        .collect();
                    E::FieldBindings::Positional(fields?)
                }
            };
            EL::Unpack(tn, tys_opt, fields)
        }
    };
    Some(sp(loc, b_))
}

enum LValue {
    Assigns(E::LValueList),
    FieldMutate(Box<E::ExpDotted>),
    Mutate(Box<E::Exp>),
}

fn lvalues(context: &mut Context, sp!(loc, e_): P::Exp) -> Option<LValue> {
    use LValue as L;
    use P::Exp_ as PE;
    let al: LValue = match e_ {
        PE::Unit => L::Assigns(sp(loc, vec![])),
        PE::ExpList(pes) => {
            let al_opt: Option<E::LValueList_> =
                pes.into_iter().map(|pe| assign(context, pe)).collect();
            L::Assigns(sp(loc, al_opt?))
        }
        PE::Dereference(pr) => {
            let er = exp(context, *pr);
            L::Mutate(er)
        }
        pdotted_ @ PE::Dot(_, _) => {
            let dotted = exp_dotted(context, sp(loc, pdotted_))?;
            L::FieldMutate(Box::new(dotted))
        }
        _ => L::Assigns(sp(loc, vec![assign(context, sp(loc, e_))?])),
    };
    Some(al)
}

fn assign(context: &mut Context, sp!(loc, e_): P::Exp) -> Option<E::LValue> {
    use E::LValue_ as EL;
    use P::Exp_ as PE;
    let a_ = match e_ {
        PE::Name(n @ sp!(_, P::NameAccessChain_::Two(_, _)), _)
        | PE::Name(n @ sp!(_, P::NameAccessChain_::Three(_, _)), _)
            if !context.in_spec_context =>
        {
            let msg = format!(
                "Unexpected assignment of module access without fields outside of a spec \
                 context.\nIf you are trying to unpack a struct, try adding fields, e.g. '{} {{}}'",
                n
            );
            context
                .env
                .add_diag(diag!(Syntax::SpecContextRestricted, (loc, msg)));

            // For unused alias warnings and unbound modules
            name_access_chain(context, Access::Term, n);

            return None;
        }
        PE::Name(n, Some(_)) if !context.in_spec_context => {
            let msg = format!(
                "Unexpected assignment of instantiated type without fields outside of a spec \
                 context.\nIf you are trying to unpack a struct, try adding fields, e.g. '{} {{}}'",
                n
            );
            context
                .env
                .add_diag(diag!(Syntax::SpecContextRestricted, (loc, msg)));

            // For unused alias warnings and unbound modules
            name_access_chain(context, Access::Term, n);

            return None;
        }
        PE::Name(pn, ptys_opt) => {
            let en = name_access_chain(context, Access::Term, pn)?;
            match &en.value {
                E::ModuleAccess_::ModuleAccess(m, n) if !context.in_spec_context => {
                    let msg = format!(
                        "Unexpected assignment of module access without fields outside of a spec \
                         context.\nIf you are trying to unpack a struct, try adding fields, e.g. \
                         '{}::{} {{}}'",
                        m, n,
                    );
                    context
                        .env
                        .add_diag(diag!(Syntax::SpecContextRestricted, (loc, msg)));
                    return None;
                }
                _ => {
                    let tys_opt = optional_types(context, ptys_opt);
                    EL::Var(None, en, tys_opt)
                }
            }
        }
        PE::Pack(pn, ptys_opt, pfields) => {
            let en = name_access_chain(context, Access::ApplyNamed, pn)?;
            let tys_opt = optional_types(context, ptys_opt);
            let efields = assign_unpack_fields(context, loc, pfields)?;
            EL::Unpack(en, tys_opt, E::FieldBindings::Named(efields))
        }
        PE::Call(pn, false, ptys_opt, sp!(_, exprs)) => {
            context
                .env
                .check_feature(FeatureGate::PositionalFields, context.current_package, loc);
            let en = name_access_chain(context, Access::ApplyNamed, pn)?;
            let tys_opt = optional_types(context, ptys_opt);
            let pfields: Option<_> = exprs.into_iter().map(|e| assign(context, e)).collect();
            EL::Unpack(en, tys_opt, E::FieldBindings::Positional(pfields?))
        }
        _ => {
            context.env.add_diag(diag!(
                Syntax::InvalidLValue,
                (
                    loc,
                    "Invalid assignment syntax. Expected: a local, a field write, or a \
                     deconstructing assignment"
                )
            ));
            return None;
        }
    };
    Some(sp(loc, a_))
}

fn assign_unpack_fields(
    context: &mut Context,
    loc: Loc,
    pfields: Vec<(Field, P::Exp)>,
) -> Option<Fields<E::LValue>> {
    let afields = pfields
        .into_iter()
        .map(|(f, e)| Some((f, assign(context, e)?)))
        .collect::<Option<_>>()?;
    Some(named_fields(
        context,
        loc,
        "deconstructing assignment",
        "assignment binding",
        afields,
    ))
}

fn mutability(context: &mut Context, loc: Loc, pmut: Mutability) -> Mutability {
    let supports_let_mut = context
        .env
        .supports_feature(context.current_package, FeatureGate::LetMut);
    match pmut {
        Some(loc) => {
            assert!(supports_let_mut, "ICE mut should not parse without let mut");
            Some(loc)
        }
        None if supports_let_mut => None,
        // without let mut enabled, all locals are mutable and do not need the annotation
        None => Some(loc),
    }
}

//**************************************************************************************************
// Unbound names
//**************************************************************************************************

fn unbound_names_spec_block(unbound: &mut BTreeSet<Name>, sp!(_, sb_): &E::SpecBlock) {
    sb_.members
        .iter()
        .for_each(|member| unbound_names_spec_block_member(unbound, member))
}

fn unbound_names_spec_block_member(unbound: &mut BTreeSet<Name>, sp!(_, m_): &E::SpecBlockMember) {
    use E::SpecBlockMember_ as M;
    match &m_ {
        M::Condition {
            exp,
            additional_exps,
            ..
        } => {
            unbound_names_exp(unbound, exp);
            additional_exps
                .iter()
                .for_each(|e| unbound_names_exp(unbound, e));
        }
        // No unbound names
        // And will error in the Move prover
        M::Function { .. }
        | M::Variable { .. }
        | M::Update { .. }
        | M::Let { .. }
        | M::Include { .. }
        | M::Apply { .. }
        | M::Pragma { .. } => (),
    }
}

fn unbound_names_exp(unbound: &mut BTreeSet<Name>, sp!(_, e_): &E::Exp) {
    use E::Exp_ as EE;
    match e_ {
        EE::Value(_)
        | EE::Break
        | EE::Continue
        | EE::UnresolvedError
        | EE::Name(sp!(_, E::ModuleAccess_::ModuleAccess(..)), _)
        | EE::Unit { .. } => (),
        EE::Copy(v) | EE::Move(v) => {
            unbound.insert(v.0);
        }
        EE::Name(sp!(_, E::ModuleAccess_::Name(n)), _) => {
            unbound.insert(*n);
        }
        EE::Call(_, _, _, sp!(_, es_)) | EE::Vector(_, _, sp!(_, es_)) => {
            unbound_names_exps(unbound, es_)
        }
        EE::MethodCall(ed, _, _, sp!(_, es_)) => {
            unbound_names_dotted(unbound, ed);
            unbound_names_exps(unbound, es_)
        }
        EE::Pack(_, _, es) => unbound_names_exps(unbound, es.iter().map(|(_, _, (_, e))| e)),
        EE::IfElse(econd, et, ef) => {
            unbound_names_exp(unbound, ef);
            unbound_names_exp(unbound, et);
            unbound_names_exp(unbound, econd)
        }
        EE::While(econd, eloop) => {
            unbound_names_exp(unbound, eloop);
            unbound_names_exp(unbound, econd)
        }
        EE::Loop(eloop) => unbound_names_exp(unbound, eloop),

        EE::Block(seq) => unbound_names_sequence(unbound, seq),
        EE::Lambda(ls, er) => {
            unbound_names_exp(unbound, er);
            // remove anything in `ls`
            unbound_names_binds(unbound, ls);
        }
        EE::Quant(_, rs, trs, cr_opt, er) => {
            unbound_names_exp(unbound, er);
            if let Some(cr) = cr_opt {
                unbound_names_exp(unbound, cr);
            }
            for tr in trs {
                unbound_names_exps(unbound, tr);
            }
            // remove anything in `rs`
            unbound_names_binds_with_range(unbound, rs);
        }
        EE::Assign(ls, er) => {
            unbound_names_exp(unbound, er);
            // remove anything in `ls`
            unbound_names_assigns(unbound, ls);
        }
        EE::Return(e)
        | EE::Abort(e)
        | EE::Dereference(e)
        | EE::UnaryExp(_, e)
        | EE::Borrow(_, e)
        | EE::Cast(e, _)
        | EE::Annotate(e, _) => unbound_names_exp(unbound, e),
        EE::FieldMutate(ed, er) => {
            unbound_names_exp(unbound, er);
            unbound_names_dotted(unbound, ed)
        }
        EE::Mutate(el, er) | EE::BinopExp(el, _, er) => {
            unbound_names_exp(unbound, er);
            unbound_names_exp(unbound, el)
        }
        EE::ExpList(es) => unbound_names_exps(unbound, es),
        EE::ExpDotted(ed) => unbound_names_dotted(unbound, ed),
        EE::Index(el, ei) => {
            unbound_names_exp(unbound, ei);
            unbound_names_exp(unbound, el)
        }

        EE::Spec(_, unbound_names) => unbound.extend(unbound_names.iter().cloned()),
    }
}

fn unbound_names_exps<'a>(unbound: &mut BTreeSet<Name>, es: impl IntoIterator<Item = &'a E::Exp>) {
    es.into_iter().for_each(|e| unbound_names_exp(unbound, e))
}

fn unbound_names_sequence(unbound: &mut BTreeSet<Name>, seq: &E::Sequence) {
    seq.1
        .iter()
        .rev()
        .for_each(|s| unbound_names_sequence_item(unbound, s))
}

fn unbound_names_sequence_item(unbound: &mut BTreeSet<Name>, sp!(_, es_): &E::SequenceItem) {
    use E::SequenceItem_ as ES;
    match es_ {
        ES::Seq(e) => unbound_names_exp(unbound, e),
        ES::Declare(ls, _) => unbound_names_binds(unbound, ls),
        ES::Bind(ls, er) => {
            unbound_names_exp(unbound, er);
            // remove anything in `ls`
            unbound_names_binds(unbound, ls);
        }
    }
}

fn unbound_names_binds(unbound: &mut BTreeSet<Name>, sp!(_, ls_): &E::LValueList) {
    ls_.iter()
        .rev()
        .for_each(|l| unbound_names_bind(unbound, l))
}

fn unbound_names_binds_with_range(
    unbound: &mut BTreeSet<Name>,
    sp!(_, rs_): &E::LValueWithRangeList,
) {
    rs_.iter().rev().for_each(|sp!(_, (b, r))| {
        unbound_names_bind(unbound, b);
        unbound_names_exp(unbound, r)
    })
}

fn unbound_names_bind(unbound: &mut BTreeSet<Name>, sp!(_, l_): &E::LValue) {
    use E::LValue_ as EL;
    match l_ {
        EL::Var(_, sp!(_, E::ModuleAccess_::Name(n)), _) => {
            unbound.remove(n);
        }
        EL::Var(_, sp!(_, E::ModuleAccess_::ModuleAccess(..)), _) => {
            // Qualified vars are not considered in unbound set.
        }
        EL::Unpack(_, _, efields) => match efields {
            E::FieldBindings::Named(efields) => efields
                .iter()
                .for_each(|(_, _, (_, l))| unbound_names_bind(unbound, l)),
            E::FieldBindings::Positional(lvals) => {
                lvals.iter().for_each(|l| unbound_names_bind(unbound, l))
            }
        },
    }
}

fn unbound_names_assigns(unbound: &mut BTreeSet<Name>, sp!(_, ls_): &E::LValueList) {
    ls_.iter()
        .rev()
        .for_each(|l| unbound_names_assign(unbound, l))
}

fn unbound_names_assign(unbound: &mut BTreeSet<Name>, sp!(_, l_): &E::LValue) {
    use E::LValue_ as EL;
    match l_ {
        EL::Var(_, sp!(_, E::ModuleAccess_::Name(n)), _) => {
            unbound.insert(*n);
        }
        EL::Var(_, sp!(_, E::ModuleAccess_::ModuleAccess(..)), _) => {
            // Qualified vars are not considered in unbound set.
        }
        EL::Unpack(_, _, efields) => match efields {
            E::FieldBindings::Named(efields) => efields
                .iter()
                .for_each(|(_, _, (_, l))| unbound_names_assign(unbound, l)),
            E::FieldBindings::Positional(lvals) => {
                lvals.iter().for_each(|l| unbound_names_assign(unbound, l))
            }
        },
    }
}

fn unbound_names_dotted(unbound: &mut BTreeSet<Name>, sp!(_, edot_): &E::ExpDotted) {
    use E::ExpDotted_ as ED;
    match edot_ {
        ED::Exp(e) => unbound_names_exp(unbound, e),
        ED::Dot(d, _) => unbound_names_dotted(unbound, d),
    }
}

//**************************************************************************************************
// Valid names
//**************************************************************************************************

fn check_valid_address_name_(
    env: &mut CompilationEnv,
    sp!(_, ln_): &P::LeadingNameAccess,
) -> Result<(), ()> {
    use P::LeadingNameAccess_ as LN;
    match ln_ {
        LN::AnonymousAddress(_) => Ok(()),
        LN::Name(n) => check_restricted_name_all_cases_(env, NameCase::Address, n),
    }
}

fn check_valid_local_name(context: &mut Context, v: &Var) {
    fn is_valid(s: Symbol) -> bool {
        s.starts_with('_') || s.starts_with(|c: char| c.is_ascii_lowercase())
    }
    if !is_valid(v.value()) {
        let msg = format!(
            "Invalid local variable name '{}'. Local variable names must start with 'a'..'z' (or \
             '_')",
            v,
        );
        context
            .env
            .add_diag(diag!(Declarations::InvalidName, (v.loc(), msg)));
    }
    let _ = check_restricted_name_all_cases(context, NameCase::Variable, &v.0);
}

#[derive(Copy, Clone, Debug)]
enum ModuleMemberKind {
    Constant,
    Function,
    Struct,
    Schema,
}

impl ModuleMemberKind {
    fn case(self) -> NameCase {
        match self {
            ModuleMemberKind::Constant => NameCase::Constant,
            ModuleMemberKind::Function => NameCase::Function,
            ModuleMemberKind::Struct => NameCase::Struct,
            ModuleMemberKind::Schema => NameCase::Schema,
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum NameCase {
    Constant,
    Function,
    Struct,
    Schema,
    Module,
    ModuleMemberAlias(ModuleMemberKind),
    ModuleAlias,
    Variable,
    Address,
}

impl NameCase {
    const fn name(&self) -> &'static str {
        match self {
            NameCase::Constant => "constant",
            NameCase::Function => "function",
            NameCase::Struct => "struct",
            NameCase::Schema => "schema",
            NameCase::Module => "module",
            NameCase::ModuleMemberAlias(ModuleMemberKind::Function) => "function alias",
            NameCase::ModuleMemberAlias(ModuleMemberKind::Constant) => "constant alias",
            NameCase::ModuleMemberAlias(ModuleMemberKind::Struct) => "struct alias",
            NameCase::ModuleMemberAlias(ModuleMemberKind::Schema) => "schema alias",
            NameCase::ModuleAlias => "module alias",
            NameCase::Variable => "variable",
            NameCase::Address => "address",
        }
    }
}

fn check_valid_module_member_name(
    context: &mut Context,
    member: ModuleMemberKind,
    name: Name,
) -> Option<Name> {
    match check_valid_module_member_name_impl(context, member, &name, member.case()) {
        Err(()) => None,
        Ok(()) => Some(name),
    }
}

fn check_valid_module_member_alias(
    context: &mut Context,
    member: ModuleMemberKind,
    alias: Name,
) -> Option<Name> {
    match check_valid_module_member_name_impl(
        context,
        member,
        &alias,
        NameCase::ModuleMemberAlias(member),
    ) {
        Err(()) => None,
        Ok(()) => Some(alias),
    }
}

fn check_valid_module_member_name_impl(
    context: &mut Context,
    member: ModuleMemberKind,
    n: &Name,
    case: NameCase,
) -> Result<(), ()> {
    use ModuleMemberKind as M;
    fn upper_first_letter(s: &str) -> String {
        let mut chars = s.chars();
        match chars.next() {
            None => String::new(),
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        }
    }
    match member {
        M::Function => {
            if n.value.starts_with(|c| c == '_') {
                let msg = format!(
                    "Invalid {} name '{}'. {} names cannot start with '_'",
                    case.name(),
                    n,
                    upper_first_letter(case.name()),
                );
                context
                    .env
                    .add_diag(diag!(Declarations::InvalidName, (n.loc, msg)));
                return Err(());
            }
        }
        M::Constant | M::Struct | M::Schema => {
            if !is_valid_struct_constant_or_schema_name(&n.value) {
                let msg = format!(
                    "Invalid {} name '{}'. {} names must start with 'A'..'Z'",
                    case.name(),
                    n,
                    upper_first_letter(case.name()),
                );
                context
                    .env
                    .add_diag(diag!(Declarations::InvalidName, (n.loc, msg)));
                return Err(());
            }
        }
    }

    // TODO move these names to a more central place?
    check_restricted_names(
        context,
        case,
        n,
        crate::naming::ast::BuiltinFunction_::all_names(),
    )?;
    check_restricted_names(
        context,
        case,
        n,
        crate::naming::ast::BuiltinTypeName_::all_names(),
    )?;

    // Restricting Self for now in the case where we ever have impls
    // Otherwise, we could allow it
    check_restricted_name_all_cases(context, case, n)?;

    Ok(())
}

pub fn is_valid_struct_constant_or_schema_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase())
}

// Checks for a restricted name in any decl case
// Self and vector are not allowed
fn check_restricted_name_all_cases(
    context: &mut Context,
    case: NameCase,
    n: &Name,
) -> Result<(), ()> {
    check_restricted_name_all_cases_(context.env, case, n)
}

fn check_restricted_name_all_cases_(
    env: &mut CompilationEnv,
    case: NameCase,
    n: &Name,
) -> Result<(), ()> {
    let n_str = n.value.as_str();
    let can_be_vector = matches!(case, NameCase::Module | NameCase::ModuleAlias);
    if n_str == ModuleName::SELF_NAME
        || (!can_be_vector && n_str == crate::naming::ast::BuiltinTypeName_::VECTOR)
    {
        env.add_diag(restricted_name_error(case, n.loc, n_str));
        Err(())
    } else {
        Ok(())
    }
}

fn check_restricted_names(
    context: &mut Context,
    case: NameCase,
    sp!(loc, n_): &Name,
    all_names: &BTreeSet<Symbol>,
) -> Result<(), ()> {
    if all_names.contains(n_) {
        context.env.add_diag(restricted_name_error(case, *loc, n_));
        Err(())
    } else {
        Ok(())
    }
}

fn restricted_name_error(case: NameCase, loc: Loc, restricted: &str) -> Diagnostic {
    let a_or_an = match case.name().chars().next().unwrap() {
        // TODO this is not exhaustive to the indefinite article rules in English
        // but 'case' is never user generated, so it should be okay for a while/forever...
        'a' | 'e' | 'i' | 'o' | 'u' => "an",
        _ => "a",
    };
    let msg = format!(
        "Invalid {case} name '{restricted}'. '{restricted}' is restricted and cannot be used to \
         name {a_or_an} {case}",
        a_or_an = a_or_an,
        case = case.name(),
        restricted = restricted,
    );
    diag!(NameResolution::ReservedName, (loc, msg))
}
