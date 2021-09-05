/* automatically generated by rust-bindgen 0.59.1 */

pub const OBJC_SMALL_OBJECT_MASK: u32 = 7;
pub const OBJC_SMALL_OBJECT_SHIFT: u32 = 3;
pub const OBJC_CAP_EXCEPTIONS: u32 = 0;
pub const OBJC_CAP_SYNCRONIZE: u32 = 1;
pub const OBJC_CAP_PROPERTIES: u32 = 2;
pub const OBJC_CAP_PROPERTY_INTROSPECTION: u32 = 3;
pub const OBJC_CAP_OPTIONAL_PROTOCOLS: u32 = 4;
pub const OBJC_CAP_NONFRAGILE_IVARS: u32 = 5;
pub const OBJC_CAP_TYPE_DEPENDENT_DISPATCH: u32 = 6;
pub const OBJC_CAP_LOW_MEMORY: u32 = 7;
pub const OBJC_DEVELOPER_MODE: u32 = 8;
pub const OBJC_UNIFIED_EXCEPTION_MODEL: u32 = 9;
pub const OBJC_CAP_REGISTERED_COMPATIBILITY_ALIASES: u32 = 10;
pub const OBJC_CAP_ARC: u32 = 11;
pub const OBJC_CAP_GARBAGE_COLLECTION: u32 = 12;
pub const OBJC_CAP_ASSOCIATED_REFERENCES: u32 = 13;
pub const OBJC_CAP_SMALL_OBJECTS: u32 = 14;
pub const OBJC_CAP_PROTOTYPES: u32 = 15;
pub const OBJC_ARC_AUTORELEASE_DEBUG: u32 = 16;
pub const OBJC_CAP_TRACING: u32 = 17;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct objc_slot2 {
    pub method: IMP,
}
extern "C" {
    pub static mut objc_method_cache_version: u64;
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct objc_slot {
    pub owner: Class,
    pub cachedFor: Class,
    pub types: *const ::std::os::raw::c_char,
    pub version: ::std::os::raw::c_int,
    pub method: IMP,
    pub selector: SEL,
}
extern "C" {
    pub fn objc_msg_lookup(arg1: id, arg2: SEL) -> IMP;
}
extern "C" {
    pub fn objc_msg_lookup_super(arg1: *mut objc_super, arg2: SEL) -> IMP;
}
extern "C" {
    pub fn objc_msg_lookup_sender(receiver: *mut id, selector: SEL, sender: id) -> *mut objc_slot;
}
extern "C" {
    pub fn objc_get_slot(arg1: Class, arg2: SEL) -> *mut objc_slot;
}
extern "C" {
    pub fn objc_get_slot2(arg1: Class, arg2: SEL, arg3: *mut u64) -> *mut objc_slot2;
}
extern "C" {
    pub fn objc_slot_lookup_version(
        receiver: *mut id,
        selector: SEL,
        arg1: *mut u64,
    ) -> *mut objc_slot2;
}
extern "C" {
    pub fn objc_msg_lookup2(receiver: *mut id, selector: SEL) -> IMP;
}
extern "C" {
    pub fn objc_set_apple_compatible_objcxx_exceptions(
        newValue: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn objc_test_capability(x: ::std::os::raw::c_int) -> ::std::os::raw::c_int;
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct objc_category {
    _unused: [u8; 0],
}
extern "C" {
    pub static mut _objc_lookup_class:
        ::std::option::Option<unsafe extern "C" fn(name: *const ::std::os::raw::c_char) -> Class>;
}
extern "C" {
    pub static mut _objc_load_callback:
        ::std::option::Option<unsafe extern "C" fn(cls: Class, category: *mut objc_category)>;
}
extern "C" {
    pub static mut objc_proxy_lookup:
        ::std::option::Option<unsafe extern "C" fn(receiver: id, op: SEL) -> id>;
}
extern "C" {
    pub static mut __objc_msg_forward3:
        ::std::option::Option<unsafe extern "C" fn(arg1: id, arg2: SEL) -> *mut objc_slot>;
}
extern "C" {
    pub static mut __objc_msg_forward2:
        ::std::option::Option<unsafe extern "C" fn(arg1: id, arg2: SEL) -> IMP>;
}
extern "C" {
    pub static mut _objc_unexpected_exception:
        ::std::option::Option<unsafe extern "C" fn(exception: id)>;
}
extern "C" {
    pub static mut _objc_class_for_boxing_foreign_exception:
        ::std::option::Option<unsafe extern "C" fn(exceptionClass: i64) -> Class>;
}
extern "C" {
    pub static mut _objc_selector_type_mismatch2: ::std::option::Option<
        unsafe extern "C" fn(cls: Class, selector: SEL, result: *mut objc_slot2) -> IMP,
    >;
}
extern "C" {
    pub static mut _objc_selector_type_mismatch: ::std::option::Option<
        unsafe extern "C" fn(cls: Class, selector: SEL, result: *mut objc_slot) -> *mut objc_slot,
    >;
}
extern "C" {
    pub static mut _objc_weak_load: ::std::option::Option<unsafe extern "C" fn(object: id) -> id>;
}
pub type objc_tracing_hook = ::std::option::Option<
    unsafe extern "C" fn(
        arg1: id,
        arg2: SEL,
        arg3: IMP,
        arg4: ::std::os::raw::c_int,
        arg5: *mut ::std::os::raw::c_void,
    ) -> IMP,
>;
extern "C" {
    pub fn objc_registerTracingHook(arg1: SEL, arg2: objc_tracing_hook) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn objc_skip_type_qualifiers(
        type_: *const ::std::os::raw::c_char,
    ) -> *const ::std::os::raw::c_char;
}
extern "C" {
    pub fn objc_skip_typespec(
        type_: *const ::std::os::raw::c_char,
    ) -> *const ::std::os::raw::c_char;
}
extern "C" {
    pub fn objc_skip_argspec(type_: *const ::std::os::raw::c_char)
        -> *const ::std::os::raw::c_char;
}
extern "C" {
    pub fn objc_sizeof_type(type_: *const ::std::os::raw::c_char) -> usize;
}
extern "C" {
    pub fn objc_alignof_type(type_: *const ::std::os::raw::c_char) -> usize;
}
extern "C" {
    pub fn objc_aligned_size(type_: *const ::std::os::raw::c_char) -> usize;
}
extern "C" {
    pub fn objc_promoted_size(type_: *const ::std::os::raw::c_char) -> usize;
}
extern "C" {
    pub fn method_get_number_of_arguments(method: *mut objc_method) -> ::std::os::raw::c_uint;
}
extern "C" {
    pub fn objc_get_type_qualifiers(type_: *const ::std::os::raw::c_char)
        -> ::std::os::raw::c_uint;
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct objc_struct_layout {
    pub original_type: *const ::std::os::raw::c_char,
    pub type_: *const ::std::os::raw::c_char,
    pub prev_type: *const ::std::os::raw::c_char,
    pub record_size: ::std::os::raw::c_uint,
    pub record_align: ::std::os::raw::c_uint,
}
extern "C" {
    pub fn objc_layout_structure(
        type_: *const ::std::os::raw::c_char,
        layout: *mut objc_struct_layout,
    );
}
extern "C" {
    pub fn objc_layout_structure_next_member(layout: *mut objc_struct_layout) -> BOOL;
}
extern "C" {
    pub fn objc_layout_structure_get_info(
        layout: *mut objc_struct_layout,
        offset: *mut ::std::os::raw::c_uint,
        align: *mut ::std::os::raw::c_uint,
        type_: *mut *const ::std::os::raw::c_char,
    );
}
