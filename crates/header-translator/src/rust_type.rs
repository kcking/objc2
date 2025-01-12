use std::str::FromStr;
use std::sync::LazyLock;
use std::{fmt, iter};

use clang::{CallingConvention, Entity, EntityKind, Nullability, Type, TypeKind};
use proc_macro2::{TokenStream, TokenTree};

use crate::context::Context;
use crate::display_helper::FormatterFn;
use crate::id::ItemIdentifier;
use crate::stmt::is_bridged;
use crate::stmt::items_required_by_decl;
use crate::thread_safety::ThreadSafety;
use crate::unexposed_attr::UnexposedAttr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ParsePosition {
    Suffix,
    Prefix,
}

impl ParsePosition {
    fn strip<'a>(self, s: &'a str, needle: &str) -> Option<&'a str> {
        match self {
            Self::Suffix => s.strip_suffix(needle),
            Self::Prefix => s.strip_prefix(needle),
        }
    }
}

/// Helper for parsing various attributes.
///
/// This is _very_ ugly, but required because libclang doesn't expose
/// lifetime information.
#[derive(Debug)]
struct AttributeParser<'a, 'b> {
    _original_name: &'a str,
    name: &'a str,
    expected_name: &'b str,
}

impl<'a, 'b> AttributeParser<'a, 'b> {
    fn new(name: &'a str, expected_name: &'b str) -> Self {
        Self {
            _original_name: name,
            name: name.trim(),
            expected_name: expected_name.trim(),
        }
    }

    fn map(&mut self, f: impl Fn(&str) -> &str) {
        self.name = f(self.name);
        self.expected_name = f(self.expected_name);
    }

    fn set_constant_array(&mut self) {
        self.map(|s| {
            let (s, _) = s.split_once('[').expect("array to contain [");
            s.trim()
        });
    }

    /// Parse an incomplete array like:
    /// `id<MTLFunctionHandle>  _Nullable const  _Nonnull __unsafe_unretained[]`
    /// By removing the ending `[]`.
    fn set_incomplete_array(&mut self) {
        self.map(|s| s.strip_suffix("[]").expect("array to end with []").trim());
    }

    /// Parse a function pointer like:
    /// `void (^ _Nonnull __strong)(...)`
    /// By extracting the inner data to:
    /// `^ _Nonnull __strong`
    fn set_fn_ptr(&mut self) {
        self.map(|s| {
            let (_, s) = s.split_once('(').expect("fn to have begin parenthesis");
            let (s, _) = s.split_once(')').expect("fn to have end parenthesis");
            s.trim()
        });
    }

    fn set_inner_pointer(&mut self) {
        if let Some(rest) = self.name.strip_suffix('*') {
            self.name = rest.trim();
        } else {
            error!(?self, "expected pointer to have star");
        }
    }
}

impl AttributeParser<'_, '_> {
    fn strip(&mut self, needle: &str, position: ParsePosition) -> bool {
        if let Some(rest) = position.strip(self.name, needle) {
            // If the string is present in the name
            if position.strip(self.expected_name, needle).is_some() {
                let rest = rest.trim();
                // If it can be stripped from both `name` and `expected_name`,
                // it might appear twice in `name`.
                //
                // This is done to support:
                // "const char * _Nonnull  _Nonnull[]".
                if position.strip(rest, needle).is_some() {
                    self.name = rest;
                    return true;
                }
            } else {
                // And _not_ in the expected name, then we should strip it so that they match.
                self.name = rest.trim();
                return true;
            }
        }

        false
    }

    fn lifetime(&mut self, position: ParsePosition) -> Lifetime {
        if self.strip("__unsafe_unretained", position) {
            Lifetime::Unretained
        } else if self.strip("__strong", position) {
            Lifetime::Strong
        } else if self.strip("__weak", position) {
            Lifetime::Weak
        } else if self.strip("__autoreleasing", position) {
            Lifetime::Autoreleasing
        } else {
            Lifetime::Unspecified
        }
    }

    /// We completely ignore `__kindof` in Rust as it is done in Swift, since
    /// it only exists to allow legacy Objective-C code to continue compiling.
    ///
    /// See <https://lapcatsoftware.com/articles/kindof.html>
    fn is_kindof(&mut self, position: ParsePosition) -> bool {
        self.strip("__kindof", position)
    }

    fn is_const(&mut self, position: ParsePosition) -> bool {
        self.strip("const", position)
    }

    fn nullability(&mut self, position: ParsePosition) -> Option<Nullability> {
        if self.strip("_Nullable", position) {
            Some(Nullability::Nullable)
        } else if self.strip("_Nonnull", position) {
            Some(Nullability::NonNull)
        } else if self.strip("_Null_unspecified", position) {
            Some(Nullability::Unspecified)
        } else {
            None
        }
    }

    fn nullable_result(&mut self, position: ParsePosition) -> bool {
        self.strip("_Nullable_result", position)
    }
}

impl Drop for AttributeParser<'_, '_> {
    fn drop(&mut self) {
        if !std::thread::panicking() && self.name != self.expected_name {
            error!(?self, "could not extract all attributes");
        }
    }
}

/// ObjCLifetime
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Lifetime {
    Unspecified,
    /// OCL_ExplicitNone
    Unretained,
    /// OCL_Strong
    Strong,
    /// OCL_Weak
    Weak,
    /// OCL_Autoreleasing
    Autoreleasing,
}

impl Lifetime {
    fn update(&mut self, new: Self) {
        match (*self, new) {
            (_, Self::Unspecified) => {
                // No lifetime attribute parsed
            }
            (Self::Unspecified, _) => {
                *self = new;
            }
            // Temporary
            (Self::Strong, Self::Strong) => {}
            (old, new) => error!(?old, ?new, "invalid lifetime update"),
        }
    }
}

// TODO: refactor this
fn update_nullability(nullability: &mut Nullability, new: Option<Nullability>) {
    match (*nullability, new) {
        (_, None) => {
            // No nullability attribute parsed
        }
        (Nullability::Unspecified, Some(new)) => {
            *nullability = new;
        }
        (old, new) => error!(?old, ?new, "invalid nullability update"),
    }
}

fn check_nullability(ty: &Type<'_>, new: Option<Nullability>) -> Nullability {
    let on_ty = ty.get_nullability();
    if new != on_ty {
        error!(?ty, ?on_ty, ?new, "failed parsing nullability");
    }
    new.unwrap_or(Nullability::Unspecified)
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum MethodArgumentQualifier {
    In,
    Inout,
    Out,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Primitive {
    Void,
    C99Bool,
    Char,
    SChar,
    UChar,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    LongLong,
    ULongLong,
    Float,
    Double,
    F32,
    F64,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    ISize,
    USize,
    PtrDiff,
    VaList,
    // Objective-C
    ObjcBool,
    NSInteger,
    NSUInteger,
    Imp,
}

impl Primitive {
    fn required_items(&self) -> Vec<ItemIdentifier> {
        match self {
            Self::ObjcBool => vec![ItemIdentifier::objc("Bool")],
            Self::NSInteger => vec![ItemIdentifier::objc("NSInteger")],
            Self::NSUInteger => vec![ItemIdentifier::objc("NSUInteger")],
            Self::Imp => vec![ItemIdentifier::objc("Imp")],
            Self::VaList => vec![ItemIdentifier::core_ffi("VaList")],
            _ => {
                let s = self.as_str();
                if s.starts_with("c_") {
                    vec![ItemIdentifier::core_ffi(s)]
                } else {
                    vec![]
                }
            }
        }
    }

    const fn as_str(&self) -> &'static str {
        match self {
            // Primitives
            Self::Void => "c_void",
            Self::C99Bool => "bool",
            Self::Char => "c_char",
            Self::SChar => "c_schar",
            Self::UChar => "c_uchar",
            Self::Short => "c_short",
            Self::UShort => "c_ushort",
            Self::Int => "c_int",
            Self::UInt => "c_uint",
            Self::Long => "c_long",
            Self::ULong => "c_ulong",
            Self::LongLong => "c_longlong",
            Self::ULongLong => "c_ulonglong",
            Self::Float => "c_float",
            Self::Double => "c_double",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I8 => "i8",
            Self::U8 => "u8",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I32 => "i32",
            Self::U32 => "u32",
            Self::I64 => "i64",
            Self::U64 => "u64",
            // TODO: Use core::ffi::c_ssize_t
            Self::ISize => "isize",
            // TODO: Use core::ffi::c_size_t
            Self::USize => "usize",
            Self::VaList => "VaList",
            // TODO: Use core::ffi::c_ptr_diff_t
            Self::PtrDiff => "isize",
            Self::ObjcBool => "Bool",
            Self::NSInteger => "NSInteger",
            Self::NSUInteger => "NSUInteger",
            // Assume nullable for now
            Self::Imp => "Option<Imp>",
        }
    }
}

impl fmt::Display for Primitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A reference to a class or a protocol declaration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemRef {
    id: ItemIdentifier,
    thread_safety: ThreadSafety,
    required_items: Vec<ItemIdentifier>,
}

impl ItemRef {
    fn required_items(&self) -> Vec<ItemIdentifier> {
        self.required_items.clone()
    }

    fn new(entity_ref: &Entity<'_>, context: &Context<'_>) -> Self {
        let entity = entity_ref
            .get_location()
            .expect("itemref location")
            .get_entity()
            .expect("itemref entity");

        let mut id = ItemIdentifier::new(&entity, context);

        if let Some(external) = context.library(id.library_name()).external.get(&id.name) {
            let id = ItemIdentifier::from_raw(id.name, external.module.clone());
            let thread_safety = external
                .thread_safety
                .as_deref()
                .map(ThreadSafety::from_string)
                .unwrap_or(ThreadSafety::dummy());
            let required_items = external
                .required_items
                .iter()
                .cloned()
                .chain(iter::once(id.clone()))
                .collect();
            return Self {
                id,
                thread_safety,
                required_items,
            };
        }

        match entity.get_kind() {
            EntityKind::ObjCInterfaceDecl | EntityKind::ObjCProtocolDecl => Self {
                id,
                thread_safety: ThreadSafety::from_decl(&entity, context),
                required_items: items_required_by_decl(&entity, context),
            },
            EntityKind::MacroExpansion => {
                id.name = entity_ref.get_name().unwrap_or_else(|| {
                    error!(?entity_ref, ?entity, "macro ref did not have name");
                    id.name
                });
                Self {
                    id: id.clone(),
                    // We cannot get thread safety from macro expansions
                    thread_safety: ThreadSafety::dummy(),
                    // Similarly, we cannot get for required items
                    required_items: vec![id],
                }
            }
            _ => {
                error!(?entity, "could not get declaration. Add appropriate external.{}.module = \"...\" to translation-config.toml", id.name);
                Self {
                    id: id.clone(),
                    thread_safety: ThreadSafety::dummy(),
                    required_items: vec![id],
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    Primitive(Primitive),
    Class {
        decl: ItemRef,
        generics: Vec<Self>,
        protocols: Vec<ItemRef>,
    },
    GenericParam {
        name: String,
    },
    AnyObject {
        protocols: Vec<ItemRef>,
    },
    AnyProtocol,
    AnyClass {
        protocols: Vec<ItemRef>,
    },
    Self_,
    Sel {
        nullability: Nullability,
    },
    Pointer {
        nullability: Nullability,
        is_const: bool,
        lifetime: Lifetime,
        pointee: Box<Self>,
    },
    // When we encounter a typedef declaration like this:
    //     typedef NSString* NSAbc;
    //
    // We emit it as:
    //     type NSAbc = NSString;
    //     struct NSAbc(NSString);
    //
    // Instead of:
    //     type NSAbc = *const NSString;
    //
    // Because that means we can use ordinary Retained<NSAbc> elsewhere.
    TypeDef {
        id: ItemIdentifier,
        nullability: Nullability,
        lifetime: Lifetime,
        to: Box<Self>,
        /// Whether the typedef's declaration is a CF-like type.
        is_cf: bool,
    },
    IncompleteArray {
        nullability: Nullability,
        is_const: bool,
        pointee: Box<Self>,
    },
    Array {
        element_type: Box<Self>,
        num_elements: usize,
    },
    RustArray {
        element_type: Box<Self>,
        num_elements: usize,
    },
    Enum {
        id: ItemIdentifier,
        ty: Box<Self>,
        // No need to store variants here, they don't matter for what the type
        // itself can do.
    },
    Struct {
        id: ItemIdentifier,
        /// FIXME: This does not work for recursive structs.
        fields: Vec<Ty>,
        /// Whether the struct's declaration has a bridge attribute.
        is_bridged: bool,
    },
    Fn {
        is_variadic: bool,
        no_escape: bool,
        arguments: Vec<Self>,
        result_type: Box<Self>,
    },
    Block {
        sendable: Option<bool>,
        no_escape: bool,
        arguments: Vec<Self>,
        result_type: Box<Self>,
    },
}

fn parse_ext_vector_type(name: &str) -> Option<Ty> {
    // Match patterns like: "__attribute__((ext_vector_type(2))) int"
    // or "int __attribute__((ext_vector_type(4)))"
    let re = regex::Regex::new(r"(.*).* __attribute__.*ext_vector_type\((\d+)\).*").unwrap();

    if let Some(captures) = re.captures(name) {
        if let (Some(size), Some(primitive)) = (captures.get(2), captures.get(1)) {
            let primitive = primitive.as_str();
            if let Ok(n) = size.as_str().parse::<usize>() {
                let ty = match primitive {
                    "float" => Primitive::Float,
                    "double" => Primitive::Double,
                    "uint" | "unsigned int" => Primitive::UInt,
                    "int" => Primitive::Int,
                    "short" => Primitive::Short,
                    "ushort" | "unsigned short" => Primitive::UShort,
                    "uchar" | "unsigned char" => Primitive::UChar,
                    "char" => Primitive::Char,
                    "long" => Primitive::Long,
                    "ulong" | "unsigned long" => Primitive::ULong,
                    "half" => Primitive::I16,
                    "_Float16" => Primitive::I16,

                    _ => {
                        error!("Unhandled ext_vector_type primtiive {primitive}");
                        return None;
                    }
                };
                return Some(Ty::RustArray {
                    element_type: Box::new(Ty::Primitive(ty)),
                    num_elements: n,
                });
            }
        }
    }
    None
}

impl Ty {
    fn parse(attributed_ty: Type<'_>, mut lifetime: Lifetime, context: &Context<'_>) -> Self {
        let mut ty = attributed_ty;
        let _span = debug_span!("ty", ?ty, ?lifetime).entered();

        let mut attributed_name = attributed_ty.get_display_name();
        let mut name = ty.get_display_name();
        // If name contains ext_vector_type attribute, parse it separately
        if name.contains("ext_vector_type") {
            if let Some(vector_ty) = parse_ext_vector_type(&name) {
                return vector_ty;
            }
        }

        let mut unexposed_nullability = None;
        let mut no_escape = false;

        while let TypeKind::Unexposed | TypeKind::Attributed = ty.get_kind() {
            if let TypeKind::Attributed = ty.get_kind() {
                ty = ty
                    .get_modified_type()
                    .expect("attributed type to have modified type");
                name = ty.get_display_name();
                continue;
            }

            if let Some(nullability) = ty.get_nullability() {
                if unexposed_nullability.is_some() {
                    error!("unexposed nullability already set");
                }
                unexposed_nullability = Some(nullability);
            }

            let (new_attributed_name, attributed_attr) = parse_unexposed_tokens(&attributed_name);
            // Also parse the expected name to ensure that the formatting that
            // TokenStream does is the same on both.
            let (new_name, attr) = parse_unexposed_tokens(&name);
            if attributed_attr != attr {
                error!(
                    ?attributed_attr,
                    ?attr,
                    "attributed attr was not equal to attr",
                );
            }

            match attr {
                Some(
                    UnexposedAttr::NonIsolated
                    | UnexposedAttr::UIActor
                    | UnexposedAttr::Sendable
                    | UnexposedAttr::NonSendable,
                ) => {
                    // Ignored for now; these are usually also emitted on the method/property,
                    // which is where they will be useful in any case.
                }
                Some(UnexposedAttr::ReturnsRetained) => {
                    lifetime = Lifetime::Strong;
                }
                Some(UnexposedAttr::ReturnsNotRetained) => {
                    lifetime = Lifetime::Autoreleasing;
                }
                Some(UnexposedAttr::NoEscape) => {
                    // TODO: Use this on Pointer and BlockPointer
                    no_escape = true;
                }
                Some(attr) => error!(?attr, "unknown attribute on type"),
                None => {}
            }

            attributed_name = new_attributed_name;
            name = new_name;

            if let Some(modified) = ty.get_modified_type() {
                ty = modified;
            } else {
                error!("expected unexposed / attributed type to have modified type");
                ty = ty.get_canonical_type();
                name = ty.get_display_name();
                break;
            }
        }

        let _span = debug_span!("ty after unexposed/attributed", ?ty).entered();

        let elaborated_ty = ty;

        if let Some(true) = ty.is_elaborated() {
            ty = ty.get_elaborated_type().expect("elaborated");
        }

        let _span = debug_span!("ty after elaborated", ?ty).entered();

        let get_is_const = |new: bool| {
            if new {
                if !attributed_ty.is_const_qualified() || ty.is_const_qualified() {
                    warn!("unnecessarily stripped const");
                }
                true
            } else {
                if attributed_ty.is_const_qualified() {
                    warn!("type was const but that could not be stripped");
                }
                // Some type kinds have `const` directly on them, instead of
                // storing it inside `Attributed`.
                //
                // TODO: Remove the need for this.
                ty.is_const_qualified()
            }
        };

        match ty.get_kind() {
            TypeKind::Void => Self::Primitive(Primitive::Void),
            TypeKind::Bool => Self::Primitive(Primitive::C99Bool),
            TypeKind::CharS | TypeKind::CharU => Self::Primitive(Primitive::Char),
            TypeKind::SChar => Self::Primitive(Primitive::SChar),
            TypeKind::UChar => Self::Primitive(Primitive::UChar),
            TypeKind::Short => Self::Primitive(Primitive::Short),
            TypeKind::UShort => Self::Primitive(Primitive::UShort),
            TypeKind::Int => Self::Primitive(Primitive::Int),
            TypeKind::UInt => Self::Primitive(Primitive::UInt),
            TypeKind::Long => Self::Primitive(Primitive::Long),
            TypeKind::ULong => Self::Primitive(Primitive::ULong),
            TypeKind::LongLong => Self::Primitive(Primitive::LongLong),
            TypeKind::ULongLong => Self::Primitive(Primitive::ULongLong),
            TypeKind::Float => Self::Primitive(Primitive::Float),
            TypeKind::Double => Self::Primitive(Primitive::Double),
            TypeKind::Record => {
                let declaration = ty.get_declaration().expect("record declaration");
                Self::Struct {
                    id: ItemIdentifier::new(&declaration, context),
                    fields: ty
                        .get_fields()
                        .expect("struct fields")
                        .into_iter()
                        .map(|field| {
                            Self::parse(
                                field.get_type().expect("struct field type"),
                                Lifetime::Unspecified,
                                context,
                            )
                        })
                        .collect(),
                    is_bridged: is_bridged(&declaration, context),
                }
            }
            TypeKind::Enum => {
                let declaration = ty.get_declaration().expect("enum declaration");
                Self::Enum {
                    id: ItemIdentifier::new(&declaration, context),
                    ty: Box::new(Ty::parse(
                        declaration
                            .get_enum_underlying_type()
                            .expect("enum underlying type"),
                        Lifetime::Unspecified,
                        context,
                    )),
                }
            }
            TypeKind::ObjCId => {
                let mut parser = AttributeParser::new(&attributed_name, "id");

                lifetime.update(parser.lifetime(ParsePosition::Prefix));

                let is_const = get_is_const(parser.is_const(ParsePosition::Suffix));
                lifetime.update(parser.lifetime(ParsePosition::Suffix));

                // TODO: Use _Nullable_result
                let _nullable_result = parser.nullable_result(ParsePosition::Suffix);

                let nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };

                Self::Pointer {
                    nullability,
                    is_const,
                    lifetime,
                    pointee: Box::new(Self::AnyObject { protocols: vec![] }),
                }
            }
            TypeKind::ObjCClass => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                let lifetime = parser.lifetime(ParsePosition::Suffix);
                let nullability = unexposed_nullability
                    .or(parser.nullability(ParsePosition::Suffix))
                    .or(ty.get_nullability())
                    .unwrap_or(Nullability::Unspecified);

                Self::Pointer {
                    nullability,
                    is_const: true,
                    lifetime,
                    pointee: Box::new(Self::AnyClass { protocols: vec![] }),
                }
            }
            TypeKind::ObjCSel => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                let nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };
                Self::Sel { nullability }
            }
            TypeKind::ObjCInterface => {
                let declaration = ty.get_declaration().expect("ObjCInterface declaration");

                if !ty.get_objc_type_arguments().is_empty() {
                    panic!("generics not empty: {ty:?}");
                }
                if !ty.get_objc_protocol_declarations().is_empty() {
                    panic!("protocols not empty: {ty:?}");
                }

                if name == "Protocol" {
                    Self::AnyProtocol
                } else {
                    let decl = ItemRef::new(&declaration, context);
                    if decl.id.name != name.strip_prefix("const ").unwrap_or(&name) {
                        error!(?name, "invalid interface name");
                    }
                    Self::Class {
                        decl,
                        protocols: vec![],
                        generics: vec![],
                    }
                }
            }
            TypeKind::ObjCObject => {
                let base_ty = ty
                    .get_objc_object_base_type()
                    .expect("object to have base type");
                let name = base_ty.get_display_name();

                let generics: Vec<_> = ty
                    .get_objc_type_arguments()
                    .into_iter()
                    .map(|param| Self::parse(param, Lifetime::Unspecified, context))
                    .collect();

                let protocols: Vec<_> = ty
                    .get_objc_protocol_declarations()
                    .into_iter()
                    .map(|entity| {
                        // ItemRef::new will fall back if we can't find it here.
                        let maybe_definition = entity.get_definition().unwrap_or(entity);
                        let mut decl = ItemRef::new(&maybe_definition, context);
                        decl.id = context.replace_protocol_name(decl.id);
                        decl
                    })
                    .collect();

                match base_ty.get_kind() {
                    TypeKind::ObjCId => {
                        assert_eq!(name, "id");

                        if !generics.is_empty() {
                            panic!("generics not empty: {ty:?}, {generics:?}");
                        }

                        Self::AnyObject { protocols }
                    }
                    TypeKind::ObjCInterface => {
                        let declaration = base_ty
                            .get_declaration()
                            .expect("ObjCObject -> ObjCInterface declaration");
                        let decl = ItemRef::new(&declaration, context);
                        if decl.id.name != name {
                            error!(?name, "ObjCObject -> ObjCInterface invalid name");
                        }

                        if !generics.is_empty() && !protocols.is_empty() {
                            panic!("got object with both protocols and generics: {name:?}, {protocols:?}, {generics:?}");
                        }

                        if generics.is_empty() && protocols.is_empty() {
                            panic!("got object with empty protocols and generics: {name:?}");
                        }

                        Self::Class {
                            decl,
                            generics,
                            protocols,
                        }
                    }
                    TypeKind::ObjCClass => {
                        assert!(generics.is_empty(), "ObjCClass with generics");

                        Self::AnyClass { protocols }
                    }
                    kind => panic!("unknown ObjCObject kind {ty:?}, {kind:?}"),
                }
            }
            TypeKind::Pointer => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                let pointee = ty.get_pointee_type().expect("pointer to have pointee");
                if let TypeKind::FunctionPrototype | TypeKind::FunctionNoPrototype =
                    pointee.get_kind()
                {
                    parser.set_fn_ptr();
                }

                let is_const = ty.is_const_qualified() || pointee.is_const_qualified();
                let nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };

                let pointee = Self::parse(pointee, Lifetime::Unspecified, context);
                Self::Pointer {
                    nullability,
                    is_const,
                    lifetime,
                    pointee: Box::new(pointee),
                }
            }
            TypeKind::BlockPointer => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                parser.set_fn_ptr();

                let is_const = get_is_const(parser.is_const(ParsePosition::Suffix));
                lifetime.update(parser.lifetime(ParsePosition::Suffix));
                let nullability = parser.nullability(ParsePosition::Suffix);
                let nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, nullability)
                };

                let ty = ty.get_pointee_type().expect("pointer type to have pointee");
                match Self::parse(ty, Lifetime::Unspecified, context) {
                    Self::Fn {
                        is_variadic: false,
                        no_escape,
                        arguments,
                        result_type,
                    } => Self::Pointer {
                        nullability,
                        is_const,
                        lifetime,
                        pointee: Box::new(Self::Block {
                            sendable: None,
                            no_escape,
                            arguments,
                            result_type,
                        }),
                    },
                    pointee => panic!("unexpected pointee in block: {pointee:?}"),
                }
            }
            TypeKind::ObjCObjectPointer => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                let is_kindof = parser.is_kindof(ParsePosition::Prefix);

                let is_const = parser.is_const(ParsePosition::Suffix) || ty.is_const_qualified();
                lifetime.update(parser.lifetime(ParsePosition::Suffix));

                // TODO: Use _Nullable_result
                let _nullable_result = parser.nullable_result(ParsePosition::Suffix);

                let mut nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };

                lifetime.update(parser.lifetime(ParsePosition::Suffix));
                drop(parser);

                let pointer_name = ty.get_display_name();
                let pointee = ty.get_pointee_type().expect("pointer type to have pointee");

                let mut ty = pointee;
                while let TypeKind::Attributed = ty.get_kind() {
                    ty = ty
                        .get_modified_type()
                        .expect("attributed type to have modified type");
                }
                let attributed_name = pointee.get_display_name();
                let name = ty.get_display_name();

                let mut parser = AttributeParser::new(&attributed_name, &name);

                let mut _is_kindof = is_kindof || parser.is_kindof(ParsePosition::Prefix);

                let pointee_is_const = parser.is_const(ParsePosition::Suffix);
                lifetime.update(parser.lifetime(ParsePosition::Suffix));
                let new = parser.nullability(ParsePosition::Suffix);
                if new != pointee.get_nullability() {
                    error!("failed parsing nullability");
                }
                update_nullability(&mut nullability, new);
                lifetime.update(parser.lifetime(ParsePosition::Suffix));

                if !is_const && pointee_is_const {
                    warn!(?ty, "pointee was const while ObjCObjectPointer was not");
                }
                drop(parser);

                let pointee_name = ty.get_display_name();
                let mut parser = AttributeParser::new(&pointer_name, &pointee_name);

                _is_kindof = parser.is_kindof(ParsePosition::Prefix);
                lifetime.update(parser.lifetime(ParsePosition::Prefix));
                lifetime.update(parser.lifetime(ParsePosition::Suffix));
                // Ignore const for now
                _ = parser.is_const(ParsePosition::Suffix);
                if !matches!(
                    pointee.get_objc_object_base_type().map(|ty| ty.get_kind()),
                    Some(TypeKind::ObjCId | TypeKind::ObjCClass)
                ) {
                    parser.set_inner_pointer();
                }
                drop(parser);

                // TODO: Maybe do something with the information in the elaborated type?
                if let Some(true) = ty.is_elaborated() {
                    ty = ty.get_elaborated_type().expect("elaborated");
                }

                Self::Pointer {
                    nullability,
                    is_const,
                    lifetime,
                    pointee: Box::new(Self::parse(ty, lifetime, context)),
                }
            }
            TypeKind::Typedef => {
                let typedef_name = ty.get_typedef_name().expect("typedef has name");
                let declaration = ty.get_declaration().expect("typedef declaration");
                assert_eq!(
                    typedef_name,
                    declaration.get_name().expect("typedef declaration name")
                );
                let to = declaration
                    .get_typedef_underlying_type()
                    .expect("typedef underlying type");
                let _span = debug_span!("typedef", ?typedef_name, ?declaration, ?to).entered();

                let mut parser = AttributeParser::new(&attributed_name, &typedef_name);
                let mut _is_kindof = parser.is_kindof(ParsePosition::Prefix);
                let is_const1 = parser.is_const(ParsePosition::Prefix);
                lifetime.update(parser.lifetime(ParsePosition::Prefix));

                let is_const2 = parser.is_const(ParsePosition::Suffix);
                lifetime.update(parser.lifetime(ParsePosition::Suffix));
                let nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };
                drop(parser);

                let is_const = if is_const1 || is_const2 {
                    if !attributed_ty.is_const_qualified()
                        && !elaborated_ty.is_const_qualified()
                        && !ty.is_const_qualified()
                    {
                        warn!(
                            ?attributed_ty,
                            ?elaborated_ty,
                            ?ty,
                            ?typedef_name,
                            ?is_const1,
                            ?is_const2,
                            attr = ?attributed_ty.is_const_qualified(),
                            elaborated = ?elaborated_ty.is_const_qualified(),
                            ty = ?ty.is_const_qualified(),
                            "typedef unnecessarily stripped const",
                        );
                    }
                    true
                } else {
                    if ty.is_const_qualified() {
                        warn!("typedef was const but that could not be stripped");
                    }
                    false
                };

                match &*typedef_name {
                    "BOOL" => return Self::Primitive(Primitive::ObjcBool),
                    "IMP" => return Self::Primitive(Primitive::Imp),

                    "int8_t" => return Self::Primitive(Primitive::I8),
                    "__int8_t" => return Self::Primitive(Primitive::I8),
                    "uint8_t" => return Self::Primitive(Primitive::U8),
                    "__uint8_t" => return Self::Primitive(Primitive::U8),
                    "int16_t" => return Self::Primitive(Primitive::I16),
                    "__int16_t" => return Self::Primitive(Primitive::I16),
                    "uint16_t" => return Self::Primitive(Primitive::U16),
                    "__uint16_t" => return Self::Primitive(Primitive::U16),
                    "int32_t" => return Self::Primitive(Primitive::I32),
                    "__int32_t" => return Self::Primitive(Primitive::I32),
                    "uint32_t" => return Self::Primitive(Primitive::U32),
                    "__uint32_t" => return Self::Primitive(Primitive::U32),
                    "int64_t" => return Self::Primitive(Primitive::I64),
                    "__int64_t" => return Self::Primitive(Primitive::I64),
                    "uint64_t" => return Self::Primitive(Primitive::U64),
                    "__uint64_t" => return Self::Primitive(Primitive::U64),
                    "ssize_t" => return Self::Primitive(Primitive::ISize),
                    "size_t" => return Self::Primitive(Primitive::USize),
                    "ptrdiff_t" => return Self::Primitive(Primitive::PtrDiff),
                    // https://github.com/rust-lang/rust/issues/65473
                    "intptr_t" => return Self::Primitive(Primitive::ISize),
                    "uintptr_t" => return Self::Primitive(Primitive::USize),

                    // Varargs, still unsupported by Rust.
                    "__builtin_va_list" => return Self::Primitive(Primitive::VaList),

                    // MacTypes.h
                    "UInt8" => return Self::Primitive(Primitive::U8),
                    "UInt16" => return Self::Primitive(Primitive::U16),
                    "UInt32" => return Self::Primitive(Primitive::U32),
                    "UInt64" => return Self::Primitive(Primitive::U64),
                    "SInt8" => return Self::Primitive(Primitive::I8),
                    "SInt16" => return Self::Primitive(Primitive::I16),
                    "SInt32" => return Self::Primitive(Primitive::I32),
                    "SInt64" => return Self::Primitive(Primitive::I64),
                    "Float32" => return Self::Primitive(Primitive::F32),
                    "Float64" => return Self::Primitive(Primitive::F64),
                    "Float80" => panic!("can't handle 80 bit MacOS float"),
                    "Float96" => panic!("can't handle 96 bit 68881 float"),

                    // Workaround for this otherwise requiring libc.
                    "dispatch_qos_class_t" => {
                        return Self::TypeDef {
                            id: ItemIdentifier::new(&declaration, context),
                            nullability,
                            lifetime,
                            to: Box::new(Self::Primitive(Primitive::Int)),
                            is_cf: false,
                        }
                    }

                    "NSInteger" => return Self::Primitive(Primitive::NSInteger),
                    "NSUInteger" => return Self::Primitive(Primitive::NSUInteger),

                    "instancetype" => {
                        return Self::Pointer {
                            nullability,
                            is_const,
                            lifetime,
                            pointee: Box::new(Self::Self_),
                        }
                    }
                    _ => {}
                }

                if let EntityKind::TemplateTypeParameter = declaration.get_kind() {
                    return Self::Pointer {
                        nullability,
                        is_const,
                        lifetime,
                        pointee: Box::new(Self::GenericParam { name: typedef_name }),
                    };
                }

                let to = Self::parse(to, Lifetime::Unspecified, context);

                let id = ItemIdentifier::new(&declaration, context);
                let is_cf = to.is_inner_cf_type(&id.name, is_bridged(&declaration, context));
                let id = context.replace_typedef_name(id, is_cf);

                Self::TypeDef {
                    id,
                    nullability,
                    lifetime,
                    to: Box::new(to),
                    is_cf,
                }
            }
            // Assume that functions without a prototype simply have 0 arguments.
            TypeKind::FunctionPrototype | TypeKind::FunctionNoPrototype => {
                let call_conv = ty.get_calling_convention().expect("fn calling convention");
                assert_eq!(
                    call_conv,
                    CallingConvention::Cdecl,
                    "fn calling convention is C"
                );

                let arguments = ty
                    .get_argument_types()
                    .expect("fn type to have argument types")
                    .into_iter()
                    .map(|ty| Self::parse(ty, Lifetime::Unspecified, context))
                    .collect();

                let result_type = ty.get_result_type().expect("fn type to have result type");
                let result_type = Self::parse(result_type, Lifetime::Unspecified, context);

                Self::Fn {
                    is_variadic: ty.get_kind() == TypeKind::FunctionPrototype && ty.is_variadic(),
                    no_escape,
                    arguments,
                    result_type: Box::new(result_type),
                }
            }
            TypeKind::IncompleteArray => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                parser.set_incomplete_array();

                let is_const = get_is_const(parser.is_const(ParsePosition::Suffix));
                lifetime.update(parser.lifetime(ParsePosition::Suffix));
                let nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };

                let ty = ty
                    .get_element_type()
                    .expect("incomplete array to have element type");

                let pointee = Self::parse(ty, lifetime, context);
                Self::IncompleteArray {
                    nullability,
                    is_const,
                    pointee: Box::new(pointee),
                }
            }
            TypeKind::ConstantArray => {
                let mut parser = AttributeParser::new(&attributed_name, &name);
                parser.set_constant_array();
                let _is_const = get_is_const(parser.is_const(ParsePosition::Suffix));
                let _nullability = if let Some(nullability) = unexposed_nullability {
                    nullability
                } else {
                    check_nullability(&attributed_ty, parser.nullability(ParsePosition::Suffix))
                };

                let element = ty.get_element_type().expect("array to have element type");
                let element_type = Self::parse(element, lifetime, context);
                let num_elements = ty
                    .get_size()
                    .expect("constant array to have element length");
                Self::Array {
                    element_type: Box::new(element_type),
                    num_elements,
                }
            }
            _ => {
                error!(?ty, "unknown type kind");
                Self::GenericParam {
                    name: "Unknown".to_string(),
                }
            }
        }
    }

    pub(crate) fn required_items(&self) -> Vec<ItemIdentifier> {
        match self {
            Self::Primitive(prim) => prim.required_items(),
            Self::Class {
                decl,
                generics,
                protocols,
            } => {
                let mut items = decl.required_items();
                for generic in generics {
                    items.extend(generic.required_items());
                }
                for protocol in protocols {
                    items.extend(protocol.required_items());
                }
                items
            }
            Self::GenericParam { .. } => Vec::new(),
            Self::AnyObject { protocols } => {
                let mut items = vec![ItemIdentifier::objc("AnyObject")];
                for protocol in protocols {
                    items.extend(protocol.required_items());
                }
                items
            }
            Self::AnyProtocol => vec![ItemIdentifier::objc("AnyProtocol")],
            Self::AnyClass { protocols } => {
                let mut items = vec![ItemIdentifier::objc("AnyClass")];
                for protocol in protocols {
                    items.extend(protocol.required_items());
                }
                items
            }
            // Methods are always emitted on an `impl`, which means that
            // `Self` is always available there, and don't required additional
            // imports, cfgs or other such things.
            Self::Self_ => Vec::new(),
            Self::Sel { .. } => vec![ItemIdentifier::objc("Sel")],
            Self::Pointer {
                pointee,
                nullability,
                ..
            }
            | Self::IncompleteArray {
                pointee,
                nullability,
                ..
            } => {
                let mut items = pointee.required_items();
                if *nullability == Nullability::NonNull {
                    items.push(ItemIdentifier::core_ptr_nonnull());
                }
                items
            }
            Self::TypeDef {
                id,
                to,
                nullability,
                ..
            } => {
                let mut items = to.required_items();
                items.push(id.clone());
                if *nullability == Nullability::NonNull {
                    items.push(ItemIdentifier::core_ptr_nonnull());
                }
                items
            }
            Self::Array { element_type, .. } => element_type.required_items(),
            Self::RustArray { element_type, .. } => element_type.required_items(),
            Self::Enum { id, ty } => {
                let mut items = ty.required_items();
                items.push(id.clone());
                items
            }
            Self::Struct { id, fields, .. } => {
                let mut items = Vec::new();
                for field in fields {
                    items.extend(field.required_items());
                }
                items.push(id.clone());
                items
            }
            Self::Fn {
                is_variadic: _,
                no_escape: _,
                arguments,
                result_type,
            } => {
                let mut items = vec![];
                for arg in arguments {
                    items.extend(arg.required_items());
                }
                items.extend(result_type.required_items());
                items
            }
            Self::Block {
                sendable: _,
                no_escape: _,
                arguments,
                result_type,
            } => {
                let mut items = vec![ItemIdentifier::block()];
                for arg in arguments {
                    items.extend(arg.required_items());
                }
                items.extend(result_type.required_items());
                items
            }
        }
    }

    /// Whether this type requires MainThreadMarker to construct.
    pub(crate) fn requires_mainthreadmarker(&self, self_requires: bool) -> bool {
        match self {
            Self::Primitive(_) => false,
            Self::Class {
                decl,
                generics,
                protocols,
            } => {
                decl.thread_safety.inferred_mainthreadonly()
                    || generics
                        .iter()
                        .any(|generic| generic.requires_mainthreadmarker(self_requires))
                    || protocols
                        .iter()
                        .any(|protocol| protocol.thread_safety.inferred_mainthreadonly())
            }
            Self::GenericParam { .. } => false,
            Self::AnyObject { protocols } => protocols
                .iter()
                .any(|protocol| protocol.thread_safety.inferred_mainthreadonly()),
            Self::AnyProtocol => false,
            Self::AnyClass { protocols } => protocols
                .iter()
                .any(|protocol| protocol.thread_safety.inferred_mainthreadonly()),
            Self::Self_ => self_requires,
            Self::Sel { .. } => false,
            Self::Pointer { pointee, .. } => pointee.requires_mainthreadmarker(self_requires),
            Self::IncompleteArray { pointee, .. } => {
                pointee.requires_mainthreadmarker(self_requires)
            }
            Self::TypeDef { to, .. } => to.requires_mainthreadmarker(self_requires),
            Self::Array { element_type, .. } => {
                element_type.requires_mainthreadmarker(self_requires)
            }
            Self::RustArray { element_type, .. } => {
                element_type.requires_mainthreadmarker(self_requires)
            }
            Self::Enum { ty, .. } => ty.requires_mainthreadmarker(self_requires),
            Self::Struct { fields, .. } => fields
                .iter()
                .any(|field| field.requires_mainthreadmarker(self_requires)),
            Self::Fn {
                is_variadic: _,
                no_escape: _,
                arguments,
                result_type,
            }
            | Self::Block {
                sendable: _,
                no_escape: _,
                arguments,
                result_type,
            } => {
                // We're overly cautious here, might be able to relax this if
                // the block is sendable.
                arguments
                    .iter()
                    .any(|arg| arg.requires_mainthreadmarker(self_requires))
                    || result_type.requires_mainthreadmarker(self_requires)
            }
        }
    }

    /// Whether this type can provide a MainThreadMarker.
    pub(crate) fn provides_mainthreadmarker(&self, self_provides: bool) -> bool {
        // Important: We mostly visit the top-level types, to not include
        // optional things like `Option<&NSView>` or `&NSArray<NSView>`.
        match self {
            Self::Class { decl, .. } => decl.thread_safety.inferred_mainthreadonly(),
            Self::AnyObject { protocols } => {
                match &**protocols {
                    [] => false,
                    [decl] => decl.thread_safety.inferred_mainthreadonly(),
                    // TODO: Handle this better
                    _ => false,
                }
            }
            Self::Self_ => self_provides,
            Self::Pointer {
                // Only visit non-null pointers
                nullability: Nullability::NonNull,
                pointee,
                ..
            } => pointee.provides_mainthreadmarker(self_provides),
            Self::TypeDef {
                // Only visit non-null typedefs
                nullability: Nullability::NonNull,
                to,
                ..
            } => to.provides_mainthreadmarker(self_provides),
            Self::Enum { ty, .. } => ty.provides_mainthreadmarker(self_provides),
            Self::Struct { fields, .. } => fields
                .iter()
                .any(|field| field.provides_mainthreadmarker(self_provides)),
            _ => false,
        }
    }

    fn inner_typedef_is_object_like(&self, in_pointer: bool) -> bool {
        match self {
            Self::Class { .. }
            | Self::GenericParam { .. }
            | Self::AnyObject { .. }
            | Self::AnyProtocol
            | Self::AnyClass { .. }
            | Self::Self_ => true,
            Self::Pointer { pointee, .. } if !in_pointer => {
                pointee.inner_typedef_is_object_like(true)
            }
            Self::TypeDef { to, .. } => to.inner_typedef_is_object_like(in_pointer),
            _ => false,
        }
    }

    /// AnyClass is safe to return as `&'static T`, since the runtime will it
    /// alive forever (and it has infinite retain count).
    ///
    /// AnyProtocol is not, though, since there's a single global object that
    /// the runtime is keeping track of, so forgetting to `release` those
    /// would leak resources.
    fn is_static_object(&self) -> bool {
        match self {
            Self::AnyClass { .. } => true,
            Self::TypeDef { to, .. } => to.is_static_object(),
            _ => false,
        }
    }

    fn is_object_like(&self) -> bool {
        match self {
            Self::Class { .. }
            | Self::GenericParam { .. }
            | Self::AnyObject { .. }
            | Self::AnyProtocol
            | Self::AnyClass { .. }
            | Self::Self_ => true,
            Self::TypeDef { to, .. } => to.inner_typedef_is_object_like(false),
            _ => false,
        }
    }

    /// Determine whether the inner type of a TypeDef is a CF-like type.
    ///
    /// Similar to what's done in Swift's implementation:
    /// <https://github.com/swiftlang/swift/blob/swift-6.0.3-RELEASE/lib/ClangImporter/CFTypeInfo.cpp#L53>
    pub(crate) fn is_inner_cf_type(&self, typedef_name: &str, typedef_is_bridged: bool) -> bool {
        // Pre-defined list of known CF types.
        // Taken from the Swift project (i.e. this is also what they do).
        static KNOWN_CF_TYPES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
            let database = include_str!("CFDatabase.def");
            let mut res = vec![];
            for item in database.split("\nCF_TYPE(").skip(1) {
                let (typename, _) = item.split_once(")").unwrap();
                res.push(typename);
            }
            res
        });

        // TODO: Figure out when to do the isCFObjectRef check that Clang does:
        // <https://github.com/llvm/llvm-project/blob/llvmorg-19.1.6/clang/lib/Analysis/CocoaConventions.cpp#L57>
        match self {
            // Recurse
            Self::TypeDef { is_cf, .. } => *is_cf,
            Self::Pointer { pointee, .. } => match &**pointee {
                // Typedefs to structs are CF types if bridged, or in
                // pre-defined list.
                Self::Struct { is_bridged, .. } => {
                    *is_bridged || KNOWN_CF_TYPES.contains(&typedef_name)
                }
                // Typedefs to void* are CF types if the typedef is
                // bridged, or in pre-defined list.
                Self::Primitive(Primitive::Void) => {
                    typedef_is_bridged || KNOWN_CF_TYPES.contains(&typedef_name)
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub(crate) fn is_cf_type(&self) -> bool {
        if let Self::TypeDef { is_cf, .. } = self {
            *is_cf
        } else {
            false
        }
    }

    pub(crate) fn is_cf_type_id(&self) -> bool {
        matches!(self, Self::TypeDef { id, .. } if id.name == "CFTypeID")
    }

    pub(crate) fn is_objc_bool(&self) -> bool {
        match self {
            Self::Primitive(Primitive::ObjcBool) => true,
            Self::TypeDef { to, .. } => to.is_objc_bool(),
            _ => false,
        }
    }

    fn plain(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| {
            match self {
                Self::Primitive(prim) => write!(f, "{prim}"),
                Self::Sel { nullability } => {
                    if *nullability == Nullability::NonNull {
                        write!(f, "Sel")
                    } else {
                        write!(f, "Option<Sel>")
                    }
                }
                Self::Pointer {
                    nullability,
                    is_const,
                    // Ignore
                    lifetime: _,
                    pointee,
                } => match &**pointee {
                    Self::Fn {
                        is_variadic,
                        no_escape: _,
                        arguments,
                        result_type,
                    } => {
                        if *nullability != Nullability::NonNull {
                            write!(f, "Option<")?;
                        }
                        // Allow pointers that the user provides to unwind.
                        //
                        // This is not _necessarily_ safe, though in practice
                        // it will be for all of Apple's frameworks.
                        write!(f, "unsafe extern \"C-unwind\" fn(")?;
                        for arg in arguments {
                            write!(f, "{},", arg.plain())?;
                        }
                        if *is_variadic {
                            write!(f, "...")?;
                        }
                        write!(f, ")")?;
                        write!(f, "{}", result_type.fn_return())?;
                        if *nullability != Nullability::NonNull {
                            write!(f, ">")?;
                        }
                        Ok(())
                    }
                    pointee => {
                        if *nullability == Nullability::NonNull {
                            write!(f, "NonNull<{}>", pointee.behind_pointer())
                        } else if *is_const {
                            write!(f, "*const {}", pointee.behind_pointer())
                        } else {
                            write!(f, "*mut {}", pointee.behind_pointer())
                        }
                    }
                },
                Self::TypeDef {
                    id,
                    nullability,
                    is_cf,
                    ..
                } if self.is_object_like() || *is_cf => {
                    if *nullability == Nullability::NonNull {
                        write!(f, "NonNull<{}>", id.path())
                    } else {
                        write!(f, "*mut {}", id.path())
                    }
                }
                Self::TypeDef { id, .. } => {
                    write!(f, "{}", id.path())
                }
                Self::IncompleteArray {
                    nullability,
                    is_const,
                    pointee,
                } => {
                    if *nullability == Nullability::NonNull {
                        write!(f, "NonNull<{}>", pointee.behind_pointer())
                    } else if *is_const {
                        write!(f, "*const {}", pointee.behind_pointer())
                    } else {
                        write!(f, "*mut {}", pointee.behind_pointer())
                    }
                }
                Self::Array {
                    element_type,
                    num_elements,
                } => write!(
                    f,
                    "ArrayUnknownABI<[{}; {num_elements}]>",
                    element_type.plain()
                ),
                Self::RustArray {
                    element_type,
                    num_elements,
                } => write!(f, "[{}; {num_elements}]", element_type.plain()),
                Self::Struct { id, .. } => {
                    write!(f, "{}", id.path())
                }
                Self::Enum { id, .. } => {
                    write!(f, "{}", id.path())
                }
                _ => {
                    error!(?self, "must be behind pointer");
                    write!(f, "{}", self.behind_pointer())
                }
            }
        })
    }

    fn behind_pointer(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Class {
                decl,
                generics,
                protocols: _,
            } => {
                write!(f, "{}", decl.id.path())?;
                if !generics.is_empty() {
                    write!(f, "<")?;
                    for generic in generics {
                        match generic {
                            Self::Pointer { pointee, .. } if pointee.is_object_like() => {
                                write!(f, "{},", pointee.behind_pointer())?
                            }
                            Self::TypeDef { id, is_cf, .. }
                                if generic.is_object_like() || *is_cf =>
                            {
                                write!(f, "{},", id.path())?
                            }
                            generic => {
                                error!(?generic, ?self, "unknown generic");
                                write!(f, "{},", generic.behind_pointer())?
                            }
                        }
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
            Self::GenericParam { name } => write!(f, "{name}"),
            Self::AnyObject { protocols } => match &**protocols {
                [] => write!(f, "AnyObject"),
                [decl] => write!(f, "ProtocolObject<dyn {}>", decl.id.path()),
                // TODO: Handle this better
                [first, rest @ ..] => {
                    write!(f, "AnyObject /* {}", first.id.path())?;
                    for protocol in rest {
                        write!(f, "+ {}", protocol.id.path())?;
                    }
                    write!(f, " */")?;
                    Ok(())
                }
            },
            Self::AnyProtocol => write!(f, "AnyProtocol"),
            Self::AnyClass { protocols } => match &**protocols {
                [] => write!(f, "AnyClass"),
                // TODO: Handle this better
                _ => write!(f, "AnyClass"),
            },
            Self::Self_ => write!(f, "Self"),
            Self::TypeDef {
                id,
                nullability,
                is_cf,
                ..
            } if *is_cf => {
                if *nullability == Nullability::NonNull {
                    write!(f, "NonNull<{}>", id.path())
                } else {
                    write!(f, "*mut {}", id.path())
                }
            }
            Self::TypeDef { id, .. } => {
                write!(f, "{}", id.path())
            }
            // TODO: Handle this better.
            Self::Fn { .. } => write!(f, "core::ffi::c_void /* TODO: Should be a function. */"),
            Self::Block {
                sendable: _,
                no_escape,
                arguments,
                result_type,
            } => {
                write!(f, "block2::Block<dyn Fn(")?;
                for arg in arguments {
                    write!(f, "{}, ", arg.plain())?;
                }
                write!(f, ")")?;
                write!(f, "{}", result_type.fn_return())?;
                if *no_escape {
                    write!(f, " + '_")?;
                } else {
                    // `dyn Fn()` in function parameters implies `+ 'static`,
                    // so no need to specify that.
                    //
                    // write!(f, " + 'static")?;
                }
                write!(f, ">")
            }
            _ => write!(f, "{}", self.plain()),
        })
    }

    pub(crate) fn method_return(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Pointer {
                nullability,
                lifetime: Lifetime::Unspecified,
                pointee,
                ..
            } if pointee.is_object_like() && !pointee.is_static_object() => {
                if *nullability == Nullability::NonNull {
                    write!(f, " -> Retained<{}>", pointee.behind_pointer())
                } else {
                    write!(f, " -> Option<Retained<{}>>", pointee.behind_pointer())
                }
            }
            Self::TypeDef {
                id,
                nullability,
                is_cf,
                ..
            } if (self.is_object_like() || *is_cf) && !self.is_static_object() => {
                // NOTE: We return CF types as `Retained` for now, since we
                // don't have support for the CF wrapper in msg_send! yet.
                if *nullability == Nullability::NonNull {
                    write!(f, " -> Retained<{}>", id.path())
                } else {
                    write!(f, " -> Option<Retained<{}>>", id.path())
                }
            }
            Self::Primitive(Primitive::C99Bool) => {
                warn!("C99's bool as Objective-C method return is ill supported");
                write!(f, " -> bool")
            }
            Self::Primitive(Primitive::ObjcBool) => write!(f, " -> bool"),
            _ => write!(f, "{}", self.fn_return()),
        })
    }

    pub(crate) fn method_return_with_error(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| {
            match self {
                Self::Pointer {
                    nullability: Nullability::Nullable,
                    lifetime: Lifetime::Unspecified,
                    is_const: false,
                    pointee,
                } if pointee.is_static_object() => {
                    // NULL -> error
                    write!(
                        f,
                        " -> Result<&'static {}, Retained<{}>>",
                        pointee.behind_pointer(),
                        ItemIdentifier::nserror().path(),
                    )
                }
                Self::Pointer {
                    nullability: Nullability::Nullable,
                    lifetime: Lifetime::Unspecified,
                    is_const: false,
                    pointee,
                } if pointee.is_object_like() => {
                    // NULL -> error
                    write!(
                        f,
                        " -> Result<Retained<{}>, Retained<{}>>",
                        pointee.behind_pointer(),
                        ItemIdentifier::nserror().path(),
                    )
                }
                Self::TypeDef {
                    id,
                    nullability: Nullability::Nullable,
                    lifetime: Lifetime::Unspecified,
                    to: _,
                    is_cf,
                } if self.is_object_like() || *is_cf => {
                    // NULL -> error
                    write!(
                        f,
                        " -> Result<Retained<{}>, Retained<{}>>",
                        id.path(),
                        ItemIdentifier::nserror().path(),
                    )
                }
                Self::Primitive(Primitive::ObjcBool) => {
                    // NO -> error
                    write!(
                        f,
                        " -> Result<(), Retained<{}>>",
                        ItemIdentifier::nserror().path()
                    )
                }
                _ => {
                    error!("unknown error result type {self:?}");
                    write!(f, "{}", self.method_return())
                }
            }
        })
    }

    pub(crate) fn method_return_encoding_type(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Primitive(Primitive::Void) => write!(f, "()"),
            Self::Primitive(Primitive::C99Bool) => write!(f, "Bool"),
            Self::Pointer { pointee, .. } if **pointee == Self::Self_ => write!(f, "*mut This"),
            _ => write!(f, "{}", self.plain()),
        })
    }

    pub(crate) fn fn_return(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| {
            if let Self::Primitive(Primitive::Void) = self {
                // Don't output anything
                return Ok(());
            }

            match self {
                Self::Pointer {
                    nullability,
                    pointee,
                    ..
                } if pointee.is_static_object() => {
                    if *nullability == Nullability::NonNull {
                        write!(f, " -> &'static {}", pointee.behind_pointer())
                    } else {
                        write!(f, " -> Option<&'static {}>", pointee.behind_pointer())
                    }
                }
                _ => write!(f, " -> {}", self.plain()),
            }
        })
    }

    pub(crate) fn fn_return_required_items(&self) -> Vec<ItemIdentifier> {
        let mut items = self.required_items();
        match self {
            Self::Pointer {
                lifetime: Lifetime::Unspecified,
                pointee,
                ..
            } if pointee.is_object_like() && !pointee.is_static_object() => {
                items.push(ItemIdentifier::objc("Retained"));
            }
            Self::TypeDef { .. } if self.is_object_like() && !self.is_static_object() => {
                items.push(ItemIdentifier::objc("Retained"));
            }
            Self::TypeDef { is_cf, .. } if *is_cf => {
                items.push(ItemIdentifier::cf("CFRetained"));
                items.push(ItemIdentifier::core_ptr_nonnull());
            }
            _ => {}
        }
        items
    }

    pub(crate) fn fn_return_converter(
        &self,
        returns_retained: bool,
    ) -> Option<(
        impl fmt::Display + '_,
        impl fmt::Display + '_,
        impl fmt::Display + '_,
    )> {
        let start = "let ret = ";
        // SAFETY: The function is marked with the correct retain semantics,
        // otherwise it'd be invalid to use from Obj-C with ARC and Swift too.
        let end_objc = |nullability| {
            match (nullability, returns_retained) {
                (Nullability::NonNull, true) => {
                    ";\nunsafe { Retained::from_raw(ret.as_ptr()) }.expect(\"function was marked as returning non-null, but actually returned NULL\")"
                }
                (Nullability::NonNull, false) => {
                    ";\nunsafe { Retained::retain_autoreleased(ret.as_ptr()) }.expect(\"function was marked as returning non-null, but actually returned NULL\")"
                }
                (_, true) => ";\nunsafe { Retained::from_raw(ret) }",
                (_, false) => ";\nunsafe { Retained::retain_autoreleased(ret) }",
            }
        };
        let end_cf = |nullability| match (nullability, returns_retained) {
            (Nullability::NonNull, true) => ";\nunsafe { CFRetained::from_raw(ret) }",
            (Nullability::NonNull, false) => ";\nunsafe { CFRetained::retain(ret) }",
            // CFRetain aborts on NULL pointers, so there's not really a more
            // efficient way to do this (except if we were to use e.g.
            // `CGColorRetain`/`CVOpenGLBufferRetain`/..., but that's a huge
            // hassle).
            (_, true) => ";\nNonNull::new(ret).map(|ret| unsafe { CFRetained::from_raw(ret) })",
            (_, false) => ";\nNonNull::new(ret).map(|ret| unsafe { CFRetained::retain(ret) })",
        };

        match self {
            _ if self.is_objc_bool() => Some((" -> bool".to_string(), "", ".as_bool()")),
            Self::Pointer {
                nullability,
                lifetime: Lifetime::Unspecified,
                pointee,
                ..
            } if pointee.is_object_like() && !pointee.is_static_object() => {
                let res = if *nullability == Nullability::NonNull {
                    format!(" -> Retained<{}>", pointee.behind_pointer())
                } else {
                    format!(" -> Option<Retained<{}>>", pointee.behind_pointer())
                };
                Some((res, start, end_objc(*nullability)))
            }
            Self::TypeDef {
                id, nullability, ..
            } if self.is_object_like() && !self.is_static_object() => {
                let res = if *nullability == Nullability::NonNull {
                    format!(" -> Retained<{}>", id.path())
                } else {
                    format!(" -> Option<Retained<{}>>", id.path())
                };
                Some((res, start, end_objc(*nullability)))
            }
            Self::TypeDef {
                id,
                nullability,
                is_cf,
                ..
            } if *is_cf => {
                let res = if *nullability == Nullability::NonNull {
                    format!(" -> CFRetained<{}>", id.path())
                } else {
                    format!(" -> Option<CFRetained<{}>>", id.path())
                };
                Some((res, start, end_cf(*nullability)))
            }
            _ => None,
        }
    }

    pub(crate) fn var(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Pointer {
                nullability,
                // `const` is irrelevant in statics since they're always
                // constant.
                is_const: _,
                lifetime: Lifetime::Strong | Lifetime::Unspecified,
                pointee,
            } if pointee.is_object_like() => {
                if *nullability == Nullability::NonNull {
                    write!(f, "&'static {}", pointee.behind_pointer())
                } else {
                    write!(f, "Option<&'static {}>", pointee.behind_pointer())
                }
            }
            Self::TypeDef {
                id,
                nullability,
                is_cf,
                ..
            } if self.is_object_like() || *is_cf => {
                if *nullability == Nullability::NonNull {
                    write!(f, "&'static {}", id.path())
                } else {
                    write!(f, "Option<&'static {}>", id.path())
                }
            }
            _ => write!(f, "{}", self.plain()),
        })
    }

    pub(crate) fn typedef(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match &self {
            Self::Pointer {
                nullability: _,
                is_const: _,
                lifetime: Lifetime::Unspecified | Lifetime::Strong,
                pointee,
            } if pointee.is_object_like() => {
                write!(f, "{}", pointee.behind_pointer())
            }
            Self::TypeDef { id, is_cf, .. } if self.is_object_like() || *is_cf => {
                write!(f, "{}", id.path())
            }
            Self::IncompleteArray { .. } => unimplemented!("incomplete array in typedef"),
            // Notice: We mark `typedefs` as-if behind a pointer
            _ => write!(f, "{}", self.behind_pointer()),
        })
    }

    pub(crate) fn fn_argument(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Pointer {
                nullability,
                is_const: _,
                lifetime: Lifetime::Unspecified | Lifetime::Strong,
                pointee,
            } if pointee.is_object_like() => {
                if *nullability == Nullability::NonNull {
                    write!(f, "&{}", pointee.behind_pointer())
                } else {
                    write!(f, "Option<&{}>", pointee.behind_pointer())
                }
            }
            Self::TypeDef {
                id,
                nullability,
                is_cf,
                ..
            } if self.is_object_like() || *is_cf => {
                if *nullability == Nullability::NonNull {
                    write!(f, "&{}", id.path())
                } else {
                    write!(f, "Option<&{}>", id.path())
                }
            }
            Self::Pointer {
                nullability,
                is_const: _,
                lifetime: _,
                pointee,
            } if matches!(**pointee, Self::AnyClass { .. } | Self::Block { .. }) => {
                if *nullability == Nullability::NonNull {
                    write!(f, "&{}", pointee.behind_pointer())
                } else {
                    write!(f, "Option<&{}>", pointee.behind_pointer())
                }
            }
            _ => write!(f, "{}", self.plain()),
        })
    }

    pub(crate) fn fn_argument_converter(
        &self,
    ) -> Option<(impl fmt::Display + '_, impl fmt::Display + '_)> {
        if self.is_objc_bool() {
            Some(("bool", "Bool::new"))
        } else {
            // TODO: Support out / autoreleasing pointers?
            None
        }
    }

    pub(crate) fn method_argument(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Primitive(Primitive::C99Bool) => {
                warn!("C99's bool as Objective-C method argument is ill supported");
                write!(f, "bool")
            }
            Self::Primitive(Primitive::ObjcBool) => {
                write!(f, "bool")
            }
            Self::Pointer {
                nullability,
                is_const: false,
                lifetime: Lifetime::Unspecified,
                pointee,
            } => match &**pointee {
                Self::Pointer {
                    nullability: inner_nullability,
                    // Don't care about the const-ness of the id.
                    is_const: _,
                    lifetime: Lifetime::Autoreleasing,
                    pointee,
                } => {
                    let tokens = if *inner_nullability == Nullability::NonNull {
                        format!("Retained<{}>", pointee.behind_pointer())
                    } else {
                        format!("Option<Retained<{}>>", pointee.behind_pointer())
                    };
                    if *nullability == Nullability::NonNull {
                        write!(f, "&mut {tokens}")
                    } else {
                        write!(f, "Option<&mut {tokens}>")
                    }
                }
                _ => write!(f, "{}", self.fn_argument()),
            },
            _ => write!(f, "{}", self.fn_argument()),
        })
    }

    pub(crate) fn method_argument_encoding_type(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Primitive(Primitive::C99Bool) => write!(f, "Bool"),
            _ => write!(f, "{}", self.plain()),
        })
    }

    pub(crate) fn struct_(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Array {
                element_type,
                num_elements,
            } => write!(f, "[{}; {num_elements}]", element_type.plain()),
            _ => write!(f, "{}", self.plain()),
        })
    }

    fn fn_contains_bool_argument(&self) -> bool {
        if let Self::Pointer { pointee, .. } = self {
            if let Self::Fn { arguments, .. } = &**pointee {
                if arguments
                    .iter()
                    .any(|arg| matches!(arg, Self::Primitive(Primitive::C99Bool)))
                {
                    return true;
                }
            }
        }
        false
    }

    pub(crate) fn struct_encoding(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Primitive(Primitive::C99Bool) => write!(f, "Encoding::Bool"),
            Self::Primitive(Primitive::Long) => write!(f, "Encoding::C_LONG"),
            Self::Primitive(Primitive::ULong) => write!(f, "Encoding::C_ULONG"),
            // TODO: Make all function pointers be encode, regardless of arguments
            Self::TypeDef { to, .. } if to.fn_contains_bool_argument() => {
                write!(f, "Encoding::Pointer(&Encoding::Unknown)")
            }
            _ => write!(f, "<{}>::ENCODING", self.struct_()),
        })
    }

    pub(crate) fn enum_(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| write!(f, "{}", self.plain()))
    }

    pub(crate) fn enum_encoding(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Primitive(Primitive::C99Bool) => write!(f, "Encoding::Bool"),
            Self::Primitive(Primitive::Long) => write!(f, "Encoding::C_LONG"),
            Self::Primitive(Primitive::ULong) => write!(f, "Encoding::C_ULONG"),
            _ => write!(f, "{}::ENCODING", self.enum_()),
        })
    }

    pub(crate) fn closed_enum_repr(&self) -> impl fmt::Display + '_ {
        FormatterFn(move |f| match self {
            Self::Primitive(Primitive::NSInteger) => write!(f, "#[repr(isize)] // NSInteger"),
            Self::Primitive(Primitive::NSUInteger) => write!(f, "#[repr(usize)] // NSUInteger"),
            Self::Primitive(Primitive::U32) => write!(f, "#[repr(u32)]"),
            _ => panic!("invalid closed enum repr: {self:?}"),
        })
    }

    pub(crate) const VOID_RESULT: Self = Self::Primitive(Primitive::Void);

    pub(crate) fn parse_method_argument(
        ty: Type<'_>,
        _qualifier: Option<MethodArgumentQualifier>,
        mut arg_sendable: Option<bool>,
        mut arg_no_escape: bool,
        context: &Context<'_>,
    ) -> Self {
        let mut ty = Self::parse(ty, Lifetime::Unspecified, context);

        match &mut ty {
            Self::Pointer { pointee, .. } => match &mut **pointee {
                Self::Block {
                    sendable,
                    no_escape,
                    ..
                } => {
                    *sendable = arg_sendable;
                    *no_escape = arg_no_escape;
                    arg_sendable = None;
                    arg_no_escape = false;
                }
                Self::Fn { no_escape, .. } => {
                    *no_escape = arg_no_escape;
                    arg_no_escape = false;
                }
                _ => {}
            },
            // Ignore typedefs for now
            Self::TypeDef { .. } => {
                arg_sendable = None;
                arg_no_escape = false;
            }
            _ => {}
        }

        if arg_sendable.is_some() {
            warn!(?ty, "did not consume sendable in argument");
        }

        if arg_no_escape {
            warn!(?ty, "did not consume no_escape in argument");
        }

        // TODO: Is the qualifier useful for anything?

        ty
    }

    pub(crate) fn parse_method_return(
        ty: Type<'_>,
        default_nonnull: bool,
        context: &Context<'_>,
    ) -> Self {
        let mut ty = Self::parse(ty, Lifetime::Unspecified, context);

        // As in `parse_property_return`, the nullability is not guaranteed by
        // the method, and can also fail in OOM situations, but that is
        // handled by `#[method_id(...)]`
        if default_nonnull {
            match &mut ty {
                Self::Pointer { nullability, .. } | Self::TypeDef { nullability, .. } => {
                    if *nullability == Nullability::Unspecified {
                        *nullability = Nullability::NonNull;
                    }
                }
                _ => warn!(?ty, "`default_nonnull` which is not an object"),
            }
        }

        ty
    }

    pub(crate) fn parse_function_argument(
        ty: Type<'_>,
        attr: Option<UnexposedAttr>,
        context: &Context<'_>,
    ) -> Self {
        match attr {
            Some(UnexposedAttr::NoEscape) => {
                // TODO: Use this if mapping `fn + context ptr` to closure.
            }
            Some(UnexposedAttr::ReturnsRetained | UnexposedAttr::ReturnsNotRetained) => {
                // TODO: Massage this into a lifetime
            }
            Some(attr) => {
                error!(?attr, "unknown attribute in function argument");
            }
            None => {}
        }
        Self::parse_method_argument(ty, None, None, false, context)
    }

    pub(crate) fn parse_function_return(ty: Type<'_>, context: &Context<'_>) -> Self {
        Self::parse_method_return(ty, false, context)
    }

    pub(crate) fn parse_typedef(ty: Type<'_>, context: &Context<'_>) -> Self {
        Self::parse(ty, Lifetime::Unspecified, context)
    }

    pub(crate) fn is_enum(&self) -> bool {
        matches!(self, Self::Enum { .. })
    }

    pub(crate) fn pointer_to_opaque_struct_or_void(&self) -> Option<Option<&str>> {
        if let Self::Pointer {
            pointee,
            is_const: _, // const-ness doesn't matter when defining the type
            nullability,
            lifetime,
        } = self
        {
            if let Self::Struct { id, fields, .. } = &**pointee {
                if fields.is_empty() {
                    // Extra checks to ensure we don't loose information
                    if *nullability != Nullability::Unspecified {
                        error!(?id, ?nullability, "opaque pointer had nullability");
                    }
                    if *lifetime != Lifetime::Unspecified {
                        error!(?id, ?lifetime, "opaque pointer had lifetime");
                    }

                    return Some(Some(&id.name));
                }
            }
            if let Self::Primitive(Primitive::Void) = &**pointee {
                return Some(None);
            }
        }
        None
    }

    pub(crate) fn parse_property(
        ty: Type<'_>,
        // Ignored; see `parse_property_return`
        _is_copy: bool,
        _sendable: Option<bool>,
        context: &Context<'_>,
    ) -> Self {
        Self::parse(ty, Lifetime::Unspecified, context)
    }

    pub(crate) fn parse_property_return(
        ty: Type<'_>,
        is_copy: bool,
        _sendable: Option<bool>,
        context: &Context<'_>,
    ) -> Self {
        let mut ty = Self::parse(ty, Lifetime::Unspecified, context);

        // `@property(copy)` is expected to always return a nonnull instance
        // (e.g. for strings it returns the empty string, while
        // `NSEnergyFormatter::numberFormatter` creates a new number object).
        //
        // So if the nullability is not specified by the type, we set it to
        // `nonnull` to get the desired return type.
        //
        // Note that we still keep the setter nullable, since the user may
        // want to rely on the "set `None` gets a default value"-behaviour.
        //
        // Note that none of this is strictly guaranteed by the method, and it
        // can also fail in OOM situations, so we must still perform an unwrap
        // to be sure (Swift also uses forced unwrapping here).
        //
        // This unwrap is done by `#[method_id(...)]` when we specify the
        // return type as `Retained`.
        if is_copy {
            match &mut ty {
                Self::Pointer { nullability, .. } | Self::TypeDef { nullability, .. } => {
                    if *nullability == Nullability::Unspecified {
                        *nullability = Nullability::NonNull;
                    }
                }
                _ => warn!(?ty, "property(copy) which is not an object"),
            }
        }

        ty
    }

    pub(crate) fn parse_struct_field(ty: Type<'_>, context: &Context<'_>) -> Self {
        Self::parse(ty, Lifetime::Unspecified, context)
    }

    fn is_primitive(&self) -> bool {
        match self {
            Self::Primitive(_) => true,
            Self::TypeDef { to, .. } => to.is_primitive(),
            _ => false,
        }
    }

    pub(crate) fn parse_enum(ty: Type<'_>, context: &Context<'_>) -> Self {
        let ty = Self::parse(ty, Lifetime::Unspecified, context);

        if !ty.is_primitive() {
            warn!(?ty, "enum type not a primitive");
        }

        ty
    }

    pub(crate) fn parse_static(ty: Type<'_>, context: &Context<'_>) -> Self {
        Self::parse(ty, Lifetime::Unspecified, context)
    }

    pub(crate) fn argument_is_error_out(&self) -> bool {
        if let Self::Pointer {
            // We always pass a place to write the error information,
            // so doesn't matter whether it's optional or not.
            nullability: Nullability::Nullable | Nullability::NonNull,
            is_const,
            lifetime: Lifetime::Unspecified,
            pointee,
        } = self
        {
            if let Self::Pointer {
                nullability: inner_nullability,
                is_const: inner_is_const,
                lifetime,
                pointee,
            } = &**pointee
            {
                if let Self::Class {
                    decl,
                    generics,
                    protocols,
                } = &**pointee
                {
                    if !decl.id.is_nserror() {
                        return false;
                    }
                    assert!(!is_const, "expected error not const {self:?}");
                    assert_eq!(
                        *inner_nullability,
                        Nullability::Nullable,
                        "invalid inner error nullability {self:?}"
                    );
                    assert!(!inner_is_const, "expected inner error not const {self:?}");

                    assert_eq!(generics, &[], "invalid error generics {self:?}");
                    assert_eq!(protocols, &[], "invalid error protocols {self:?}");
                    assert_eq!(
                        *lifetime,
                        Lifetime::Autoreleasing,
                        "invalid error lifetime {self:?}"
                    );
                    return true;
                }
            }
        }
        false
    }

    pub(crate) fn is_retainable(&self) -> bool {
        match self {
            Self::Pointer { pointee, .. }
                if pointee.is_object_like() && !pointee.is_static_object() =>
            {
                true
            }
            Self::TypeDef { is_cf, .. }
                if (self.is_object_like() || *is_cf) && !self.is_static_object() =>
            {
                true
            }
            _ => false,
        }
    }

    pub(crate) fn is_instancetype(&self) -> bool {
        matches!(self, Self::Pointer { pointee, .. } if **pointee == Self::Self_)
    }

    pub(crate) fn is_typedef_to(&self, s: &str) -> bool {
        matches!(self, Self::TypeDef { id, .. } if id.name == s)
    }

    pub(crate) fn is_struct(&self, s: &str) -> bool {
        matches!(self, Self::Struct { id, .. } if id.name == s)
    }

    pub(crate) fn is_enum_through_typedef(&self) -> bool {
        match self {
            Self::Enum { .. } => true,
            Self::TypeDef { to, .. } => to.is_enum_through_typedef(),
            _ => false,
        }
    }

    pub(crate) fn is_floating_through_typedef(&self) -> bool {
        match self {
            Self::Primitive(
                Primitive::F32 | Primitive::F64 | Primitive::Float | Primitive::Double,
            ) => true,
            Self::TypeDef { to, .. } => to.is_floating_through_typedef(),
            _ => false,
        }
    }

    pub(crate) fn try_fix_related_result_type(&mut self) {
        if let Self::Pointer { pointee, .. } = self {
            if let Self::AnyObject { protocols } = &**pointee {
                if !protocols.is_empty() {
                    warn!(?pointee, "related result type with protocols");
                    return;
                }

                **pointee = Self::Self_;
            } else {
                // Only fix if the type is `id`
            }
        } else {
            panic!("tried to fix related result type on non-id type")
        }
    }

    pub(crate) fn fix_fn_first_argument_cf_nullability(&mut self, fn_name: &str) {
        if let Self::TypeDef {
            id,
            nullability: nullability @ Nullability::Unspecified,
            is_cf: true,
            ..
        } = self
        {
            let type_name = id.name.strip_suffix("Ref").unwrap_or(&id.name);
            // We don't ever want to mark these as non-NULL, as they have NULL
            // statics (`kCFAllocatorDefault` and `kODSessionDefault`).
            if fn_name.contains(type_name) && !matches!(type_name, "ODSession" | "CFAllocator") {
                // Is likely a getter, so let's mark it as non-null (CF will
                // usually crash if given an unexpected NULL pointer, but
                // we're not entirely sure it will always do so).
                *nullability = Nullability::NonNull;
            }
        }
    }
}

/// Strip macros from unexposed types.
///
/// These appear in newer clang versions.
/// We should be able to extract data from the following macros if desired:
/// - NS_SWIFT_NAME
/// - NS_SWIFT_UNAVAILABLE
/// - NS_REFINED_FOR_SWIFT
/// - ...
fn parse_unexposed_tokens(s: &str) -> (String, Option<UnexposedAttr>) {
    let tokens = TokenStream::from_str(s).expect("parse attributed name");
    let mut iter = tokens.into_iter().peekable();
    let attr = if let Some(TokenTree::Ident(ident)) = iter.peek() {
        let ident = ident.to_string();
        if let Ok(attr) = UnexposedAttr::from_name(&ident, || {
            iter.next();
            if let Some(TokenTree::Group(group)) = iter.peek() {
                Some(group)
            } else {
                error!(?ident, "expected group in macro");
                None
            }
        }) {
            iter.next();
            attr
        } else {
            None
        }
    } else {
        None
    };
    (TokenStream::from_iter(iter).to_string(), attr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_unexposed_tokens() {
        fn check(inp: &str, expected: &str) {
            let (actual, attr) = parse_unexposed_tokens(inp);
            assert_eq!(actual, expected);
            assert_eq!(attr, None);
        }

        check("NS_RETURNS_INNER_POINTER const char *", "const char *");
        check(
            "API_UNAVAILABLE(macos) NSString *const __strong",
            "NSString * const __strong",
        );
        check("NS_REFINED_FOR_SWIFT NSNumber *", "NSNumber *");
        check(
            "API_AVAILABLE(macos(10.9)) const NSProgressUserInfoKey __strong",
            "const NSProgressUserInfoKey __strong",
        );
        check(
            "NS_SWIFT_NAME(replacementIndex) const NSAttributedStringKey __strong",
            "const NSAttributedStringKey __strong",
        );
        check(
            "API_DEPRECATED(\"\", macos(10.0, 10.5)) NSString *const __strong",
            "NSString * const __strong",
        );
        check(
            "API_DEPRECATED_WITH_REPLACEMENT(\"@\\\"com.adobe.encapsulated-postscript\\\"\", macos(10.0,10.14)) NSPasteboardType __strong",
            "NSPasteboardType __strong",
        );

        let (actual, attr) = parse_unexposed_tokens("NS_SWIFT_NONISOLATED NSTextAttachment *");
        assert_eq!(actual, "NSTextAttachment *");
        assert_eq!(attr, Some(UnexposedAttr::NonIsolated));

        let (actual, attr) = parse_unexposed_tokens("NS_SWIFT_UI_ACTOR SEL");
        assert_eq!(actual, "SEL");
        assert_eq!(attr, Some(UnexposedAttr::UIActor));
    }

    #[test]
    fn test_nested_typedef_is_object_like() {
        let ty = Ty::TypeDef {
            id: ItemIdentifier::dummy(),
            nullability: Nullability::Unspecified,
            lifetime: Lifetime::Unspecified,
            to: Box::new(Ty::TypeDef {
                id: ItemIdentifier::dummy(),
                nullability: Nullability::Unspecified,
                lifetime: Lifetime::Unspecified,
                to: Box::new(Ty::Pointer {
                    nullability: Nullability::Unspecified,
                    is_const: false,
                    lifetime: Lifetime::Unspecified,
                    pointee: Box::new(Ty::Class {
                        decl: ItemRef {
                            id: ItemIdentifier::dummy(),
                            thread_safety: ThreadSafety::dummy(),
                            required_items: vec![],
                        },
                        generics: vec![],
                        protocols: vec![],
                    }),
                }),
                is_cf: false,
            }),
            is_cf: false,
        };

        assert!(ty.is_object_like());
    }
}
