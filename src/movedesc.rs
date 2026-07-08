// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! Move package → `sui.rpc.v2` descriptor conversions, for `MovePackageService`.
//!
//! Builds the proto `Package` / `DatatypeDescriptor` / `FunctionDescriptor`
//! surfaces from a package's on-fork bytecode, so `GetPackage`, `GetDatatype`
//! and `GetFunction` work against forked (and locally-published) packages.
//! The bytecode is walked with `move-binary-format`; there is no node call.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use move_binary_format::CompiledModule;
use move_binary_format::file_format::{
    Ability as MvAbility, AbilitySet, DatatypeTyParameter, FunctionDefinition, SignatureToken,
    StructDefinition, Visibility,
};
use sui_rpc::proto::sui::rpc::v2 as proto;
use sui_types::move_package::MovePackage;

/// Map a package's `(module, datatype)` to the storage id of the package
/// version that first defined it (a type's `defining_id`).
fn defining_ids(pkg: &MovePackage) -> BTreeMap<(String, String), String> {
    pkg.type_origin_table()
        .iter()
        .map(|o| {
            (
                (o.module_name.clone(), o.datatype_name.clone()),
                o.package.to_hex_literal(),
            )
        })
        .collect()
}

/// Build the full proto `Package` for `pkg`, including per-module datatype and
/// function descriptors parsed from bytecode (the base object→proto conversion
/// only carries module bytecode + linkage).
pub fn build_package(pkg: &MovePackage) -> Result<proto::Package> {
    let defining = defining_ids(pkg);
    let fallback_id = pkg.original_package_id().to_hex_literal();
    let define = |module: &str, name: &str| -> String {
        defining
            .get(&(module.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| fallback_id.clone())
    };

    let mut modules = Vec::new();
    for (name, bytes) in pkg.serialized_module_map() {
        let cm = CompiledModule::deserialize_with_defaults(bytes)
            .with_context(|| format!("deserializing module {name}"))?;
        let (datatypes, functions) = module_descriptors(&cm, &define);
        let mut module = proto::Module::default();
        module.name = Some(name.clone());
        module.contents = Some(bytes.clone().into());
        module.datatypes = datatypes;
        module.functions = functions;
        modules.push(module);
    }

    let mut package = proto::Package::default();
    package.storage_id = Some(pkg.id().to_hex_literal());
    package.original_id = Some(pkg.original_package_id().to_hex_literal());
    package.version = Some(pkg.version().value());
    package.modules = modules;
    package.type_origins = pkg
        .type_origin_table()
        .iter()
        .map(|o| {
            let mut t = proto::TypeOrigin::default();
            t.module_name = Some(o.module_name.clone());
            t.datatype_name = Some(o.datatype_name.clone());
            t.package_id = Some(o.package.to_hex_literal());
            t
        })
        .collect();
    package.linkage = pkg
        .linkage_table()
        .iter()
        .map(|(original, info)| {
            let mut l = proto::Linkage::default();
            l.original_id = Some(original.to_hex_literal());
            l.upgraded_id = Some(info.upgraded_id.to_hex_literal());
            l.upgraded_version = Some(info.upgraded_version.value());
            l
        })
        .collect();
    Ok(package)
}

/// Find a single datatype (struct or enum) descriptor by module + name.
pub fn find_datatype(
    pkg: &MovePackage,
    module: &str,
    name: &str,
) -> Result<Option<proto::DatatypeDescriptor>> {
    let Some(bytes) = pkg.serialized_module_map().get(module) else {
        return Ok(None);
    };
    let cm = CompiledModule::deserialize_with_defaults(bytes)
        .with_context(|| format!("deserializing module {module}"))?;
    let defining = defining_ids(pkg);
    let fallback_id = pkg.original_package_id().to_hex_literal();
    let define = |m: &str, n: &str| -> String {
        defining
            .get(&(m.to_string(), n.to_string()))
            .cloned()
            .unwrap_or_else(|| fallback_id.clone())
    };
    let (datatypes, _) = module_descriptors(&cm, &define);
    Ok(datatypes
        .into_iter()
        .find(|d| d.name.as_deref() == Some(name)))
}

/// Find a single function descriptor by module + name.
pub fn find_function(
    pkg: &MovePackage,
    module: &str,
    name: &str,
) -> Result<Option<proto::FunctionDescriptor>> {
    let Some(bytes) = pkg.serialized_module_map().get(module) else {
        return Ok(None);
    };
    let cm = CompiledModule::deserialize_with_defaults(bytes)
        .with_context(|| format!("deserializing module {module}"))?;
    let fallback_id = pkg.original_package_id().to_hex_literal();
    let defining = defining_ids(pkg);
    let define = |m: &str, n: &str| -> String {
        defining
            .get(&(m.to_string(), n.to_string()))
            .cloned()
            .unwrap_or_else(|| fallback_id.clone())
    };
    let (_, functions) = module_descriptors(&cm, &define);
    Ok(functions
        .into_iter()
        .find(|f| f.name.as_deref() == Some(name)))
}

/// Build every datatype and function descriptor of one compiled module.
fn module_descriptors(
    cm: &CompiledModule,
    define: &dyn Fn(&str, &str) -> String,
) -> (
    Vec<proto::DatatypeDescriptor>,
    Vec<proto::FunctionDescriptor>,
) {
    let module_name = cm.self_id().name().to_string();

    let mut datatypes = Vec::new();
    for sdef in cm.struct_defs() {
        datatypes.push(struct_descriptor(cm, sdef, &module_name, define));
    }
    for edef in cm.enum_defs() {
        datatypes.push(enum_descriptor(cm, edef, &module_name, define));
    }

    let functions = cm
        .function_defs()
        .iter()
        .map(|fdef| function_descriptor(cm, fdef))
        .collect();

    (datatypes, functions)
}

fn struct_descriptor(
    cm: &CompiledModule,
    sdef: &StructDefinition,
    module_name: &str,
    define: &dyn Fn(&str, &str) -> String,
) -> proto::DatatypeDescriptor {
    let handle = cm.datatype_handle_at(sdef.struct_handle);
    let name = cm.identifier_at(handle.name).to_string();
    let fields = sdef
        .fields()
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(pos, f)| {
            field_descriptor(
                cm,
                cm.identifier_at(f.name).to_string(),
                pos,
                &f.signature.0,
            )
        })
        .collect();

    let mut d = base_datatype(
        cm,
        module_name,
        &name,
        handle.abilities,
        &handle.type_parameters,
        define,
    );
    d.kind = Some(proto::datatype_descriptor::DatatypeKind::Struct as i32);
    d.fields = fields;
    d
}

fn enum_descriptor(
    cm: &CompiledModule,
    edef: &move_binary_format::file_format::EnumDefinition,
    module_name: &str,
    define: &dyn Fn(&str, &str) -> String,
) -> proto::DatatypeDescriptor {
    let handle = cm.datatype_handle_at(edef.enum_handle);
    let name = cm.identifier_at(handle.name).to_string();
    let variants = edef
        .variants
        .iter()
        .enumerate()
        .map(|(pos, v)| {
            let fields = v
                .fields
                .iter()
                .enumerate()
                .map(|(fpos, f)| {
                    field_descriptor(
                        cm,
                        cm.identifier_at(f.name).to_string(),
                        fpos,
                        &f.signature.0,
                    )
                })
                .collect();
            let mut vd = proto::VariantDescriptor::default();
            vd.name = Some(cm.identifier_at(v.variant_name).to_string());
            vd.position = Some(pos as u32);
            vd.fields = fields;
            vd
        })
        .collect();

    let mut d = base_datatype(
        cm,
        module_name,
        &name,
        handle.abilities,
        &handle.type_parameters,
        define,
    );
    d.kind = Some(proto::datatype_descriptor::DatatypeKind::Enum as i32);
    d.variants = variants;
    d
}

fn base_datatype(
    _cm: &CompiledModule,
    module_name: &str,
    name: &str,
    abilities: AbilitySet,
    type_params: &[DatatypeTyParameter],
    define: &dyn Fn(&str, &str) -> String,
) -> proto::DatatypeDescriptor {
    let defining_id = define(module_name, name);
    let mut d = proto::DatatypeDescriptor::default();
    d.type_name = Some(format!("{defining_id}::{module_name}::{name}"));
    d.defining_id = Some(defining_id);
    d.module = Some(module_name.to_string());
    d.name = Some(name.to_string());
    d.abilities = ability_set(abilities);
    d.type_parameters = type_params
        .iter()
        .map(|p| {
            let mut tp = proto::TypeParameter::default();
            tp.constraints = ability_set(p.constraints);
            tp.is_phantom = Some(p.is_phantom);
            tp
        })
        .collect();
    d
}

fn field_descriptor(
    cm: &CompiledModule,
    name: String,
    position: usize,
    token: &SignatureToken,
) -> proto::FieldDescriptor {
    let mut f = proto::FieldDescriptor::default();
    f.name = Some(name);
    f.position = Some(position as u32);
    f.r#type = Some(open_signature_body(cm, token));
    f
}

fn function_descriptor(
    cm: &CompiledModule,
    fdef: &FunctionDefinition,
) -> proto::FunctionDescriptor {
    let handle = cm.function_handle_at(fdef.function);
    let visibility = match fdef.visibility {
        Visibility::Private => proto::function_descriptor::Visibility::Private,
        Visibility::Public => proto::function_descriptor::Visibility::Public,
        Visibility::Friend => proto::function_descriptor::Visibility::Friend,
    };
    let mut f = proto::FunctionDescriptor::default();
    f.name = Some(cm.identifier_at(handle.name).to_string());
    f.visibility = Some(visibility as i32);
    f.is_entry = Some(fdef.is_entry);
    f.type_parameters = handle
        .type_parameters
        .iter()
        .map(|constraints| {
            let mut tp = proto::TypeParameter::default();
            tp.constraints = ability_set(*constraints);
            tp.is_phantom = Some(false);
            tp
        })
        .collect();
    f.parameters = cm
        .signature_at(handle.parameters)
        .0
        .iter()
        .map(|t| open_signature(cm, t))
        .collect();
    f.returns = cm
        .signature_at(handle.return_)
        .0
        .iter()
        .map(|t| open_signature(cm, t))
        .collect();
    f
}

/// A function parameter / return signature (reference + body).
fn open_signature(cm: &CompiledModule, token: &SignatureToken) -> proto::OpenSignature {
    use proto::open_signature::Reference;
    let mut sig = proto::OpenSignature::default();
    match token {
        SignatureToken::Reference(inner) => {
            sig.reference = Some(Reference::Immutable as i32);
            sig.body = Some(open_signature_body(cm, inner));
        }
        SignatureToken::MutableReference(inner) => {
            sig.reference = Some(Reference::Mutable as i32);
            sig.body = Some(open_signature_body(cm, inner));
        }
        other => {
            sig.body = Some(open_signature_body(cm, other));
        }
    }
    sig
}

/// A (reference-free) type body: field types and the inside of references.
fn open_signature_body(cm: &CompiledModule, token: &SignatureToken) -> proto::OpenSignatureBody {
    use proto::open_signature_body::Type;
    let mut body = proto::OpenSignatureBody::default();
    match token {
        SignatureToken::Bool => body.r#type = Some(Type::Bool as i32),
        SignatureToken::U8 => body.r#type = Some(Type::U8 as i32),
        SignatureToken::U16 => body.r#type = Some(Type::U16 as i32),
        SignatureToken::U32 => body.r#type = Some(Type::U32 as i32),
        SignatureToken::U64 => body.r#type = Some(Type::U64 as i32),
        SignatureToken::U128 => body.r#type = Some(Type::U128 as i32),
        SignatureToken::U256 => body.r#type = Some(Type::U256 as i32),
        SignatureToken::Address => body.r#type = Some(Type::Address as i32),
        // `signer` never appears as a field/datatype type param; the proto body
        // enum has no signer, so fall back to the unset/unknown type.
        SignatureToken::Signer => body.r#type = Some(Type::Unknown as i32),
        SignatureToken::Vector(inner) => {
            body.r#type = Some(Type::Vector as i32);
            body.type_parameter_instantiation = vec![open_signature_body(cm, inner)];
        }
        SignatureToken::TypeParameter(i) => {
            body.r#type = Some(Type::Parameter as i32);
            body.type_parameter = Some(*i as u32);
        }
        SignatureToken::Datatype(idx) => {
            body.r#type = Some(Type::Datatype as i32);
            body.type_name = Some(datatype_name(cm, *idx));
        }
        SignatureToken::DatatypeInstantiation(boxed) => {
            let (idx, args) = boxed.as_ref();
            body.r#type = Some(Type::Datatype as i32);
            body.type_name = Some(datatype_name(cm, *idx));
            body.type_parameter_instantiation =
                args.iter().map(|t| open_signature_body(cm, t)).collect();
        }
        // References cannot appear inside a body; unwrap defensively.
        SignatureToken::Reference(inner) | SignatureToken::MutableReference(inner) => {
            return open_signature_body(cm, inner);
        }
    }
    body
}

/// Fully-qualified `<address>::<module>::<name>` for a datatype handle.
fn datatype_name(
    cm: &CompiledModule,
    idx: move_binary_format::file_format::DatatypeHandleIndex,
) -> String {
    let handle = cm.datatype_handle_at(idx);
    let module_handle = cm.module_handle_at(handle.module);
    let address = cm.address_identifier_at(module_handle.address);
    let module = cm.identifier_at(module_handle.name);
    let name = cm.identifier_at(handle.name);
    format!("{}::{}::{}", address.to_hex_literal(), module, name)
}

/// Move `AbilitySet` → the repeated proto `Ability` enum values.
fn ability_set(set: AbilitySet) -> Vec<i32> {
    set.into_iter()
        .map(|a| {
            match a {
                MvAbility::Copy => proto::Ability::Copy,
                MvAbility::Drop => proto::Ability::Drop,
                MvAbility::Store => proto::Ability::Store,
                MvAbility::Key => proto::Ability::Key,
            }
            .into()
        })
        .collect()
}
