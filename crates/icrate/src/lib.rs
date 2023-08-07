//! # Bindings to Apple's frameworks
//!
//! This crate is a mostly autogenerated interface to some of Apple's
//! frameworks. It builds upon [`objc2`] to provide memory management and
//! safety in many areas; see that crate for more details.

#![no_std]
#![cfg_attr(feature = "unstable-docsrs", feature(doc_auto_cfg))]
#![warn(elided_lifetimes_in_paths)]
#![warn(missing_copy_implementations)]
#![warn(missing_debug_implementations)]
#![deny(non_ascii_idents)]
#![warn(unreachable_pub)]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::cargo)]
#![warn(clippy::ptr_as_ptr)]
#![allow(clippy::upper_case_acronyms)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::identity_op)]
#![allow(clippy::missing_safety_doc)]
// Update in Cargo.toml as well.
#![doc(html_root_url = "https://docs.rs/icrate/0.0.4")]
#![recursion_limit = "512"]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
#[cfg(all(
    feature = "Foundation",
    feature = "Foundation_NSString",
    feature = "Foundation_NSArray"
))]
extern "C" {}

#[cfg(feature = "objective-c")]
pub extern crate objc2;

#[cfg(feature = "block")]
pub extern crate block2;

mod common;
#[macro_use]
mod macros;

// Frameworks
#[cfg(feature = "Accessibility")]
pub mod Accessibility;
#[cfg(feature = "AdServices")]
pub mod AdServices;
#[cfg(feature = "AdSupport")]
pub mod AdSupport;
#[cfg(feature = "AppKit")]
pub mod AppKit;
#[cfg(feature = "AuthenticationServices")]
pub mod AuthenticationServices;
#[cfg(feature = "AutomaticAssessmentConfiguration")]
pub mod AutomaticAssessmentConfiguration;
#[cfg(feature = "Automator")]
pub mod Automator;
#[cfg(feature = "BackgroundAssets")]
pub mod BackgroundAssets;
#[cfg(feature = "BackgroundTasks")]
pub mod BackgroundTasks;
#[cfg(feature = "BusinessChat")]
pub mod BusinessChat;
#[cfg(feature = "CallKit")]
pub mod CallKit;
#[cfg(feature = "ClassKit")]
pub mod ClassKit;
#[cfg(feature = "CloudKit")]
pub mod CloudKit;
#[cfg(feature = "Contacts")]
pub mod Contacts;
#[cfg(feature = "CoreAnimation")]
pub mod CoreAnimation;
#[cfg(feature = "CoreData")]
pub mod CoreData;
#[cfg(feature = "CoreLocation")]
pub mod CoreLocation;
#[cfg(feature = "DataDetection")]
pub mod DataDetection;
#[cfg(feature = "DeviceCheck")]
pub mod DeviceCheck;
#[cfg(feature = "EventKit")]
pub mod EventKit;
#[cfg(feature = "ExceptionHandling")]
pub mod ExceptionHandling;
#[cfg(feature = "ExtensionKit")]
pub mod ExtensionKit;
#[cfg(feature = "ExternalAccessory")]
pub mod ExternalAccessory;
#[cfg(feature = "FileProvider")]
pub mod FileProvider;
#[cfg(feature = "FileProviderUI")]
pub mod FileProviderUI;
#[cfg(feature = "Foundation")]
pub mod Foundation;
#[cfg(feature = "GameController")]
pub mod GameController;
#[cfg(feature = "GameKit")]
pub mod GameKit;
#[cfg(feature = "HealthKit")]
pub mod HealthKit;
#[cfg(feature = "IdentityLookup")]
pub mod IdentityLookup;
#[cfg(feature = "InputMethodKit")]
pub mod InputMethodKit;
#[cfg(feature = "LinkPresentation")]
pub mod LinkPresentation;
#[cfg(feature = "LocalAuthentication")]
pub mod LocalAuthentication;
#[cfg(feature = "LocalAuthenticationEmbeddedUI")]
pub mod LocalAuthenticationEmbeddedUI;
#[cfg(feature = "MailKit")]
pub mod MailKit;
#[cfg(feature = "MapKit")]
pub mod MapKit;
#[cfg(feature = "MediaPlayer")]
pub mod MediaPlayer;
#[cfg(feature = "Metal")]
pub mod Metal;
#[cfg(feature = "MetalFX")]
pub mod MetalFX;
#[cfg(feature = "MetalKit")]
pub mod MetalKit;
#[cfg(feature = "MetricKit")]
pub mod MetricKit;
#[cfg(feature = "OSAKit")]
pub mod OSAKit;
#[cfg(feature = "PhotoKit")]
pub mod PhotoKit;
#[cfg(feature = "SoundAnalysis")]
pub mod SoundAnalysis;
#[cfg(feature = "Speech")]
pub mod Speech;
#[cfg(feature = "StoreKit")]
pub mod StoreKit;
#[cfg(feature = "UniformTypeIdentifiers")]
pub mod UniformTypeIdentifiers;
#[cfg(feature = "UserNotifications")]
pub mod UserNotifications;
#[cfg(feature = "WebKit")]
pub mod WebKit;

/// Deprecated alias of [`Foundation::ns_string`].
#[macro_export]
#[deprecated = "use icrate::Foundation::ns_string instead"]
#[cfg(feature = "Foundation_NSString")]
macro_rules! ns_string {
    ($s:expr) => {
        $crate::Foundation::ns_string!($s)
    };
}
