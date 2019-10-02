// Copyright 2018 astonbitecode
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{fs, mem};
use std::any::Any;
use std::convert::TryFrom;
use std::ops::Drop;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc::{channel, Receiver, Sender};

use fs_extra::dir::get_dir_content;
use jni_sys::{
    self,
    JavaVM,
    JavaVMInitArgs,
    JavaVMOption,
    JNI_EDETACHED,
    JNI_EEXIST,
    JNI_EINVAL,
    JNI_ENOMEM,
    JNI_ERR,
    JNI_EVERSION,
    JNI_FALSE,
    JNI_OK,
    JNI_TRUE,
    JNI_VERSION_1_8,
    JNIEnv,
    jobject,
    jsize,
    jstring,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json;

use crate::{api_tweaks as tweaks, MavenSettings};
use crate::cache;
use crate::errors;
use crate::errors::{J4RsError, opt_to_res};
use crate::jni_utils;
use crate::provisioning::{get_maven_settings, JavaArtifact, LocalJarArtifact, MavenArtifact};
use crate::provisioning;
use crate::utils;

use super::logger::{debug, error, info, warn};

// Initialize the environment
include!(concat!(env!("OUT_DIR"), "/j4rs_init.rs"));

pub type Callback = fn(Jvm, Instance) -> ();

/// Holds the assets for the JVM
#[derive(Clone)]
pub struct Jvm {
    pub(crate) jni_env: *mut JNIEnv,
    detach_thread_on_drop: bool,
}
impl Jvm {
    /// Creates a new Jvm.
    pub fn new(jvm_options: &[String], lib_name_to_load: Option<String>) -> errors::Result<Jvm> {
        Self::create_jvm(jvm_options, lib_name_to_load)
    }

    /// Attaches the current thread to an active JavaVM
    pub fn attach_thread() -> errors::Result<Jvm> {
        Self::create_jvm(&Vec::new(), None)
    }

    /// If true, the thread will not be detached when the Jvm is eing dropped.
    /// This is useful when creating a Jvm while on a Thread that is created in the Java world.
    /// When this Jvm is dropped, we don't want to detach the thread from the Java VM.
    ///
    /// It prevents errors like: `attempting to detach while still running code`
    pub fn detach_thread_on_drop(&mut self, detach: bool) {
        self.detach_thread_on_drop = detach;
    }

    /// Creates a new Jvm.
    /// If a JavaVM is already created by the current process, it attempts to attach the current thread to it.
    fn create_jvm(jvm_options: &[String], lib_name_to_load: Option<String>) -> errors::Result<Jvm> {
        debug("Creating a Jvm");
        let mut jvm: *mut JavaVM = ptr::null_mut();
        let mut jni_environment: *mut JNIEnv = ptr::null_mut();

        // Create the Jvm atomically
        let _g = cache::MUTEX.lock()?;

        let result = if let Some(env) = cache::get_thread_local_env_opt() {
            info("A JVM is already created for this thread. Retrieving it...");
            jni_environment = env;

            JNI_OK
        } else {
            let created_vm = Self::get_created_vm();

            let res_int = if created_vm.is_some() {
                debug("A JVM is already created by another thread. Retrieving it...");
                jni_environment = created_vm.unwrap();

                JNI_OK
            } else {
                info("No JVMs exist. Creating a new one...");
                let mut jvm_options_vec: Vec<JavaVMOption> = jvm_options
                    .iter()
                    .map(|opt| {
                        let cstr = utils::to_c_string(opt);
                        let jo = JavaVMOption {
                            optionString: utils::to_c_string(opt),
                            extraInfo: ptr::null_mut() as *mut c_void,
                        };
                        utils::drop_c_string(cstr);
                        jo
                    })
                    .collect();

                let mut jvm_arguments = JavaVMInitArgs {
                    version: JNI_VERSION_1_8,
                    nOptions: jvm_options.len() as i32,
                    options: jvm_options_vec.as_mut_ptr(),
                    ignoreUnrecognized: JNI_FALSE,
                };

                tweaks::create_java_vm(
                    &mut jvm,
                    (&mut jni_environment as *mut *mut JNIEnv) as *mut *mut c_void,
                    (&mut jvm_arguments as *mut JavaVMInitArgs) as *mut c_void,
                )
            };

            res_int
        };

        if result != JNI_OK {
            let error_message = match result {
                JNI_EDETACHED => "thread detached from the JVM",
                JNI_EEXIST => "JVM already created",
                JNI_EINVAL => "invalid arguments",
                JNI_ENOMEM => "not enough memory",
                JNI_ERR => "unknown error",
                JNI_EVERSION => "JNI version error",
                _ => "unknown JNI error value",
            };

            Err(errors::J4RsError::JavaError(format!("Could not create the JVM: {}", error_message).to_string()))
        } else {
            let jvm = Self::try_from(jni_environment)?;
            if let Some(libname) = lib_name_to_load {
                // Pass to the Java world the name of the j4rs library.
                debug(&format!("Initializing NativeCallbackSupport with libname {}", libname));
                jvm.invoke_static("org.astonbitecode.j4rs.api.invocation.NativeCallbackToRustChannelSupport",
                                  "initialize",
                                  &vec![InvocationArg::try_from(libname)?])?;
            }

            Ok(jvm)
        }
    }

    pub fn try_from(jni_environment: *mut JNIEnv) -> errors::Result<Jvm> {
        unsafe {
            let gmid = cache::get_jni_get_method_id().or_else(|| cache::set_jni_get_method_id((**jni_environment).GetMethodID));
            let gsmid = cache::get_jni_get_static_method_id().or_else(|| cache::set_jni_get_static_method_id((**jni_environment).GetStaticMethodID));
            let _ = cache::get_jni_new_object().or_else(|| cache::set_jni_new_object((**jni_environment).NewObject));
            let _ = cache::get_jni_new_string_utf().or_else(|| cache::set_jni_new_string_utf((**jni_environment).NewStringUTF));
            let _ = cache::get_jni_get_string_utf_chars().or_else(|| cache::set_jni_get_string_utf_chars((**jni_environment).GetStringUTFChars));
            let _ = cache::get_jni_call_object_method().or_else(|| cache::set_jni_call_object_method((**jni_environment).CallObjectMethod));
            let _ = cache::get_jni_call_void_method().or_else(|| cache::set_jni_call_void_method((**jni_environment).CallVoidMethod));
            let _ = cache::get_jni_call_static_object_method().or_else(|| cache::set_jni_call_static_object_method((**jni_environment).CallStaticObjectMethod));
            let _ = cache::get_jni_new_object_array().or_else(|| cache::set_jni_new_object_array((**jni_environment).NewObjectArray));
            let _ = cache::get_jni_set_object_array_element().or_else(|| cache::set_jni_set_object_array_element((**jni_environment).SetObjectArrayElement));
            let ec = cache::get_jni_exception_check().or_else(|| cache::set_jni_exception_check((**jni_environment).ExceptionCheck));
            let ed = cache::get_jni_exception_describe().or_else(|| cache::set_jni_exception_describe((**jni_environment).ExceptionDescribe));
            let exclear = cache::get_jni_exception_clear().or_else(|| cache::set_jni_exception_clear((**jni_environment).ExceptionClear));
            let _ = cache::get_jni_delete_local_ref().or_else(|| cache::set_jni_delete_local_ref((**jni_environment).DeleteLocalRef));
            let _ = cache::get_jni_delete_global_ref().or_else(|| cache::set_jni_delete_global_ref((**jni_environment).DeleteGlobalRef));
            let _ = cache::get_jni_new_global_ref().or_else(|| cache::set_jni_new_global_ref((**jni_environment).NewGlobalRef));

            match (gmid, gsmid, ec, ed, exclear) {
                (Some(gmid), Some(gsmid), Some(ec), Some(ed), Some(exclear)) => {
                    // This is the factory class. It creates instances using reflection. Currently the `NativeInstantiationImpl`
                    let factory_class = if let Some(j) = cache::get_factory_class() {
                        j
                    } else {
                        let j = tweaks::find_class(jni_environment, cache::INST_CLASS_NAME);
                        cache::set_factory_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };

                    // The constructor of `NativeInstantiationImpl`
                    let _ = if let Some(j) = cache::get_factory_constructor_method() {
                        j
                    } else {
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string("()V");
                        // The constructor of `NativeInstantiationImpl`
                        let j = (gmid)(
                            jni_environment,
                            factory_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_factory_constructor_method(j)
                    };

                    // The class of the `InvocationArg`
                    let invocation_arg_class = if let Some(j) = cache::get_invocation_arg_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "org/astonbitecode/j4rs/api/dtos/InvocationArg",
                        );
                        cache::set_invocation_arg_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };

                    let instantiate_method_signature = format!(
                        "(Ljava/lang/String;[Lorg/astonbitecode/j4rs/api/dtos/InvocationArg;)L{};",
                        cache::INVO_IFACE_NAME);
                    let create_for_static_method_signature = format!(
                        "(Ljava/lang/String;)L{};",
                        cache::INVO_IFACE_NAME);
                    let create_java_array_method_signature = format!(
                        "(Ljava/lang/String;[Lorg/astonbitecode/j4rs/api/dtos/InvocationArg;)L{};",
                        cache::INVO_IFACE_NAME);
                    let create_java_list_method_signature = format!(
                        "(Ljava/lang/String;[Lorg/astonbitecode/j4rs/api/dtos/InvocationArg;)L{};",
                        cache::INVO_IFACE_NAME);

                    // The method id of the `instantiate` method of the `NativeInstantiation`
                    let _ = if let Some(j) = cache::get_factory_instantiate_method() {
                        j
                    } else {
                        let cstr1 = utils::to_c_string("instantiate");
                        let cstr2 = utils::to_c_string(&instantiate_method_signature);
                        let j = (gsmid)(
                            jni_environment,
                            factory_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_factory_instantiate_method(j)
                    };

                    // The method id of the `createForStatic` method of the `NativeInstantiation`
                    if cache::get_factory_create_for_static_method().is_none() {
                        let cstr1 = utils::to_c_string("createForStatic");
                        let cstr2 = utils::to_c_string(&create_for_static_method_signature);
                        let j = (gsmid)(
                            jni_environment,
                            factory_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_factory_create_for_static_method(j)
                    }

                    // The method id of the `createJavaArray` method of the `NativeInstantiation`
                    if cache::get_factory_create_java_array_method().is_none() {
                        let cstr1 = utils::to_c_string("createJavaArray");
                        let cstr2 = utils::to_c_string(&create_java_array_method_signature);
                        let j = (gsmid)(
                            jni_environment,
                            factory_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_factory_create_java_array_method(j)
                    }

                    // The method id of the `createJavaList` method of the `NativeInstantiation`
                    if cache::get_factory_create_java_list_method().is_none() {
                        let cstr1 = utils::to_c_string("createJavaList");
                        let cstr2 = utils::to_c_string(&create_java_list_method_signature);
                        let j = (gsmid)(
                            jni_environment,
                            factory_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_factory_create_java_list_method(j)
                    }

                    // The `NativeInvocationBase class`
                    let optional_class = if cfg!(target_os = "android") {
                        let native_invocation_base_class = if let Some(j) = cache::get_native_invocation_base_class() {
                            j
                        } else {
                            let j = tweaks::find_class(
                                jni_environment,
                                cache::INVO_BASE_NAME,
                            );
                            cache::set_native_invocation_base_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                        };
                        Some(native_invocation_base_class)
                    } else {
                        None
                    };

                    // The `NativeInvocation class`
                    let native_invocation_class = if let Some(j) = cache::get_native_invocation_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            cache::INVO_IFACE_NAME,
                        );
                        cache::set_native_invocation_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };

                    // The invoke method
                    if cache::get_invoke_method().is_none() {
                        let invoke_method_signature = format!(
                            "(Ljava/lang/String;[Lorg/astonbitecode/j4rs/api/dtos/InvocationArg;)L{};",
                            cache::INVO_IFACE_NAME);
                        // Get the method ID for the `NativeInvocation.invoke`
                        let cstr1 = utils::to_c_string("invoke");
                        let cstr2 = utils::to_c_string(invoke_method_signature.as_ref());
                        let j = (gmid)(
                            jni_environment,
                            native_invocation_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_invoke_method(j)
                    }

                    // The invokeStatic method
                    if cache::get_invoke_static_method().is_none() {
                        let invoke_static_method_signature = format!(
                            "(Ljava/lang/String;[Lorg/astonbitecode/j4rs/api/dtos/InvocationArg;)L{};",
                            cache::INVO_IFACE_NAME);
                        let cstr1 = utils::to_c_string("invokeStatic");
                        let cstr2 = utils::to_c_string(invoke_static_method_signature.as_ref());
                        // Get the method ID for the `NativeInvocation.invokeStatic`
                        let j = (gmid)(
                            jni_environment,
                            native_invocation_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_invoke_static_method(j)
                    }

                    // The invoke to channel method
                    if cache::get_invoke_to_channel_method().is_none() {
                        let invoke_to_channel_method_signature = "(JLjava/lang/String;[Lorg/astonbitecode/j4rs/api/dtos/InvocationArg;)V";
                        let cstr1 = utils::to_c_string("invokeToChannel");
                        let cstr2 = utils::to_c_string(&invoke_to_channel_method_signature);
                        // Get the method ID for the `NativeInvocation.invokeToChannel`
                        let j = (gmid)(
                            jni_environment,
                            native_invocation_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_invoke_to_channel_method(j)
                    };

                    // The init callback channel method
                    if cache::get_init_callback_channel_method().is_none() {
                        let init_callback_channel_method_signature = "(J)V";
                        let cstr1 = utils::to_c_string("initializeCallbackChannel");
                        let cstr2 = utils::to_c_string(&init_callback_channel_method_signature);
                        // Get the method ID for the `NativeInvocation.initializeCallbackChannel`
                        let j = (gmid)(
                            jni_environment,
                            native_invocation_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_init_callback_channel_method(j)
                    };

                    // The field method
                    if cache::get_field_method().is_none() {
                        let field_method_signature = format!(
                            "(Ljava/lang/String;)L{};",
                            cache::INVO_IFACE_NAME);
                        let cstr1 = utils::to_c_string("field");
                        let cstr2 = utils::to_c_string(field_method_signature.as_ref());
                        // Get the method ID for the `NativeInvocation.field`
                        let j = (gmid)(
                            jni_environment,
                            native_invocation_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_field_method(j)
                    };

                    // The class to invoke the cloneInstance into is not the same in Android target os.
                    // The native_invocation_base_class is checked first because of Java7 compatibility issues in Android.
                    // In Java8 and later, the static implementation in the interfaces is used. This is not supported in Java7
                    // and there is a base class created for this reason.
                    let class_to_invoke_clone_and_cast = if let Some(j) = cache::get_class_to_invoke_clone_and_cast() {
                        j
                    } else {
                        let j = optional_class.unwrap_or(native_invocation_class);
                        cache::set_class_to_invoke_clone_and_cast(j);
                        j
                    };

                    // The clone method
                    if cache::get_clone_static_method().is_none() {
                        let clone_method_signature = format!(
                            "(L{};)L{};",
                            cache::INVO_IFACE_NAME,
                            cache::INVO_IFACE_NAME);
                        let cstr1 = utils::to_c_string("cloneInstance");
                        let cstr2 = utils::to_c_string(clone_method_signature.as_ref());
                        // Get the method ID for the `NativeInvocation.clone`
                        let j = (gsmid)(
                            jni_environment,
                            class_to_invoke_clone_and_cast,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_clone_static_method(j)
                    };

                    // The cast method
                    if cache::get_cast_static_method().is_none() {
                        let cast_method_signature = format!(
                            "(L{};Ljava/lang/String;)L{};",
                            cache::INVO_IFACE_NAME,
                            cache::INVO_IFACE_NAME);
                        let cstr1 = utils::to_c_string("cast");
                        let cstr2 = utils::to_c_string(cast_method_signature.as_ref());

                        // Get the method ID for the `NativeInvocation.cast`
                        let j = (gsmid)(
                            jni_environment,
                            class_to_invoke_clone_and_cast,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_cast_static_method(j)
                    };

                    // The getJson method
                    if cache::get_get_json_method().is_none() {
                        let get_json_method_signature = "()Ljava/lang/String;";
                        let cstr1 = utils::to_c_string("getJson");
                        let cstr2 = utils::to_c_string(get_json_method_signature.as_ref());

                        // Get the method ID for the `NativeInvocation.getJson`
                        let j = (gmid)(
                            jni_environment,
                            native_invocation_class,
                            cstr1,
                            cstr2,
                        );
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_get_json_method(j)
                    };

                    // The constructor of `InvocationArg` for Java created args
                    if cache::get_inv_arg_java_constructor_method().is_none() {
                        let inv_arg_java_constructor_method_signature = format!("(Ljava/lang/String;L{};)V", cache::INVO_IFACE_NAME);
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&inv_arg_java_constructor_method_signature);
                        let j = (gmid)(
                            jni_environment,
                            invocation_arg_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_inv_arg_java_constructor_method(j)
                    };

                    // The constructor of `InvocationArg` for Rust created args
                    if cache::get_inv_arg_rust_constructor_method().is_none() {
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string("(Ljava/lang/String;Ljava/lang/String;)V");
                        let j = (gmid)(
                            jni_environment,
                            invocation_arg_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_inv_arg_rust_constructor_method(j)
                    };

                    // The constructor of `InvocationArg` for basic object type instances created by Rust via JNI
                    if cache::get_inv_arg_basic_rust_constructor_method().is_none() {
                        let inv_arg_basic_rust_constructor_method_signature = "(Ljava/lang/String;Ljava/lang/Object;)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&inv_arg_basic_rust_constructor_method_signature);
                        let j = (gmid)(
                            jni_environment,
                            invocation_arg_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_inv_arg_basic_rust_constructor_method(j)
                    };

                    // The `Integer class`
                    let integer_class = if let Some(j) = cache::get_integer_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "java/lang/Integer",
                        );
                        cache::set_integer_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };
                    // The constructor used for the creation of Integers
                    if cache::get_integer_constructor_method().is_none() {
                        let constructor_signature = "(I)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&constructor_signature);
                        let j = (gmid)(
                            jni_environment,
                            integer_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_integer_constructor_method(j)
                    };

                    // The `Long class`
                    let long_class = if let Some(j) = cache::get_long_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "java/lang/Long",
                        );
                        cache::set_long_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };
                    // The constructor used for the creation of Longs
                    if cache::get_long_constructor_method().is_none() {
                        let constructor_signature = "(J)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&constructor_signature);
                        let j = (gmid)(
                            jni_environment,
                            long_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_long_constructor_method(j)
                    };

                    // The `Short class`
                    let short_class = if let Some(j) = cache::get_short_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "java/lang/Short",
                        );
                        cache::set_short_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };
                    // The constructor used for the creation of Shorts
                    if cache::get_short_constructor_method().is_none() {
                        let constructor_signature = "(S)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&constructor_signature);
                        let j = (gmid)(
                            jni_environment,
                            short_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_short_constructor_method(j)
                    };

                    // The `Byte class`
                    let byte_class = if let Some(j) = cache::get_byte_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "java/lang/Byte",
                        );
                        cache::set_byte_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };
                    // The constructor used for the creation of Bytes
                    if cache::get_byte_constructor_method().is_none() {
                        let constructor_signature = "(B)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&constructor_signature);
                        let j = (gmid)(
                            jni_environment,
                            byte_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_byte_constructor_method(j)
                    };

                    // The `Float class`
                    let float_class = if let Some(j) = cache::get_float_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "java/lang/Float",
                        );
                        cache::set_float_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };
                    // The constructor used for the creation of Floats
                    if cache::get_float_constructor_method().is_none() {
                        let constructor_signature = "(F)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&constructor_signature);
                        let j = (gmid)(
                            jni_environment,
                            float_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_float_constructor_method(j)
                    };

                    // The `Float class`
                    let double_class = if let Some(j) = cache::get_double_class() {
                        j
                    } else {
                        let j = tweaks::find_class(
                            jni_environment,
                            "java/lang/Double",
                        );
                        cache::set_double_class(jni_utils::create_global_ref_from_local_ref(j, jni_environment)?)
                    };
                    // The constructor used for the creation of Floats
                    if cache::get_double_constructor_method().is_none() {
                        let constructor_signature = "(D)V";
                        let cstr1 = utils::to_c_string("<init>");
                        let cstr2 = utils::to_c_string(&constructor_signature);
                        let j = (gmid)(
                            jni_environment,
                            double_class,
                            cstr1,
                            cstr2);
                        utils::drop_c_string(cstr1);
                        utils::drop_c_string(cstr2);
                        cache::set_double_constructor_method(j)
                    };

                    if (ec)(jni_environment) == JNI_TRUE {
                        (ed)(jni_environment);
                        (exclear)(jni_environment);
                        Err(errors::J4RsError::JavaError("The VM cannot be started... Please check the logs.".to_string()))
                    } else {
                        let jvm = Jvm {
                            jni_env: jni_environment,
                            detach_thread_on_drop: true,
                        };

                        if cache::get_thread_local_env_opt().is_none() {
                            cache::set_thread_local_env(Some(jni_environment));
                        }
                        cache::add_active_jvm();

                        Ok(jvm)
                    }
                }
                (_, _, _, _, _) => {
                    Err(errors::J4RsError::JniError(format!("Could not initialize the JVM: Error while trying to retrieve JNI functions.")))
                }
            }
        }
    }

    /// Creates an `Instance` of the class `class_name`, passing an array of `InvocationArg`s to construct the instance.
    pub fn create_instance(&self, class_name: &str, inv_args: &[InvocationArg]) -> errors::Result<Instance> {
        debug(&format!("Instantiating class {} using {} arguments", class_name, inv_args.len()));
        unsafe {
            // Factory invocation - first argument: create a jstring to pass as argument for the class_name
            let class_name_jstring: jstring = jni_utils::global_jobject_from_str(&class_name, self.jni_env)?;

            // Factory invocation - rest of the arguments: Create a new objectarray of class InvocationArg
            let size = inv_args.len() as i32;
            let array_ptr = {
                let j = (opt_to_res(cache::get_jni_new_object_array())?)(
                    self.jni_env,
                    size,
                    opt_to_res(cache::get_invocation_arg_class())?,
                    ptr::null_mut(),
                );
                jni_utils::create_global_ref_from_local_ref(j, self.jni_env)?
            };
            let mut inv_arg_jobjects: Vec<jobject> = Vec::new();

            // Factory invocation - rest of the arguments: populate the array
            for i in 0..size {
                // Create an InvocationArg Java Object
                let inv_arg_java = inv_args[i as usize].as_java_ptr(self.jni_env)?;
                // Set it in the array
                (opt_to_res(cache::get_jni_set_object_array_element())?)(
                    self.jni_env,
                    array_ptr,
                    i,
                    inv_arg_java,
                );
                inv_arg_jobjects.push(inv_arg_java);
            }
            // Call the method of the factory that instantiates a new class of `class_name`.
            // This returns a NativeInvocation that acts like a proxy to the Java world.
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                self.jni_env,
                opt_to_res(cache::get_factory_class())?,
                opt_to_res(cache::get_factory_instantiate_method())?,
                class_name_jstring,
                array_ptr,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            let native_invocation_global_instance = jni_utils::create_global_ref_from_local_ref(native_invocation_instance, self.jni_env)?;
            // Prevent memory leaks from the created local references
            jni_utils::delete_java_ref(self.jni_env, array_ptr);
            jni_utils::delete_java_ref(self.jni_env, class_name_jstring);
            for inv_arg_jobject in inv_arg_jobjects {
                jni_utils::delete_java_ref(self.jni_env, inv_arg_jobject);
            }

            // Create and return the Instance
            Self::do_return(self.jni_env, Instance {
                jinstance: native_invocation_global_instance,
                class_name: class_name.to_string(),
            })
        }
    }

    /// Retrieves the static class `class_name`.
    pub fn static_class(&self, class_name: &str) -> errors::Result<Instance> {
        debug(&format!("Retrieving static class {}", class_name));
        unsafe {
            // Factory invocation - first argument: create a jstring to pass as argument for the class_name
            let class_name_jstring: jstring = jni_utils::global_jobject_from_str(&class_name, self.jni_env)?;

            // Call the method of the factory that creates a NativeInvocation for static calls to methods of class `class_name`.
            // This returns a NativeInvocation that acts like a proxy to the Java world.
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                self.jni_env,
                opt_to_res(cache::get_factory_class())?,
                opt_to_res(cache::get_factory_create_for_static_method())?,
                class_name_jstring,
            );

            jni_utils::delete_java_ref(self.jni_env, class_name_jstring);

            // Create and return the Instance. The Instance::from transforms the passed instance to a global one. No need to transform it here as well.
            Self::do_return(self.jni_env, Instance::from(native_invocation_instance)?)
        }
    }

    /// Creates a new Java Array with elements of the class `class_name`.
    /// The array will have the `InvocationArg`s populated.
    /// The `InvocationArg`s __must__ be of type _class_name_.
    pub fn create_java_array(&self, class_name: &str, inv_args: &[InvocationArg]) -> errors::Result<Instance> {
        debug(&format!("Creating a java array of class {} with {} elements", class_name, inv_args.len()));
        unsafe {
            // Factory invocation - first argument: create a jstring to pass as argument for the class_name
            let class_name_jstring: jstring = jni_utils::global_jobject_from_str(&class_name, self.jni_env)?;

            // Factory invocation - rest of the arguments: Create a new objectarray of class InvocationArg
            let size = inv_args.len() as i32;
            let array_ptr = {
                let j = (opt_to_res(cache::get_jni_new_object_array())?)(
                    self.jni_env,
                    size,
                    opt_to_res(cache::get_invocation_arg_class())?,
                    ptr::null_mut(),
                );
                jni_utils::create_global_ref_from_local_ref(j, self.jni_env)?
            };
            let mut inv_arg_jobjects: Vec<jobject> = Vec::new();

            // Factory invocation - rest of the arguments: populate the array
            for i in 0..size {
                // Create an InvocationArg Java Object
                let inv_arg_java = inv_args[i as usize].as_java_ptr(self.jni_env)?;
                // Set it in the array
                (opt_to_res(cache::get_jni_set_object_array_element())?)(
                    self.jni_env,
                    array_ptr,
                    i,
                    inv_arg_java,
                );
                inv_arg_jobjects.push(inv_arg_java);
            }
            // Call the method of the factory that instantiates a new Java Array of `class_name`.
            // This returns a NativeInvocation that acts like a proxy to the Java world.
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                self.jni_env,
                opt_to_res(cache::get_factory_class())?,
                opt_to_res(cache::get_factory_create_java_array_method())?,
                class_name_jstring,
                array_ptr,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            let native_invocation_global_instance = jni_utils::create_global_ref_from_local_ref(native_invocation_instance, self.jni_env)?;
            // Prevent memory leaks from the created local references
            for inv_arg_jobject in inv_arg_jobjects {
                jni_utils::delete_java_ref(self.jni_env, inv_arg_jobject);
            }
            jni_utils::delete_java_ref(self.jni_env, array_ptr);
            jni_utils::delete_java_ref(self.jni_env, class_name_jstring);

            // Create and return the Instance
            Self::do_return(self.jni_env, Instance {
                jinstance: native_invocation_global_instance,
                class_name: class_name.to_string(),
            })
        }
    }

    /// Creates a new Java List with elements of the class `class_name`.
    /// The array will have the `InvocationArg`s populated.
    /// The `InvocationArg`s __must__ be of type _class_name_.
    pub fn create_java_list(&self, class_name: &str, inv_args: &[InvocationArg]) -> errors::Result<Instance> {
        Jvm::do_create_java_list(self.jni_env, class_name, inv_args)
    }

    fn do_create_java_list(jni_env: *mut JNIEnv, class_name: &str, inv_args: &[InvocationArg]) -> errors::Result<Instance> {
        debug(&format!("Creating a java list of class {} with {} elements", class_name, inv_args.len()));
        unsafe {
            // Factory invocation - first argument: create a jstring to pass as argument for the class_name
            let class_name_jstring: jstring = jni_utils::global_jobject_from_str(&class_name, jni_env)?;

            // Factory invocation - rest of the arguments: Create a new object list of class InvocationArg
            let size = inv_args.len() as i32;
            let array_ptr = {
                let j = (opt_to_res(cache::get_jni_new_object_array())?)(
                    jni_env,
                    size,
                    opt_to_res(cache::get_invocation_arg_class())?,
                    ptr::null_mut(),
                );
                jni_utils::create_global_ref_from_local_ref(j, jni_env)?
            };
            let mut inv_arg_jobjects: Vec<jobject> = Vec::new();

            // Factory invocation - rest of the arguments: populate the array
            for i in 0..size {
                // Create an InvocationArg Java Object
                let inv_arg_java = inv_args[i as usize].as_java_ptr(jni_env)?;
                // Set it in the array
                (opt_to_res(cache::get_jni_set_object_array_element())?)(
                    jni_env,
                    array_ptr,
                    i,
                    inv_arg_java,
                );
                inv_arg_jobjects.push(inv_arg_java);
            }
            // Call the method of the factory that instantiates a new Java Array of `class_name`.
            // This returns a NativeInvocation that acts like a proxy to the Java world.
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                jni_env,
                opt_to_res(cache::get_factory_class())?,
                opt_to_res(cache::get_factory_create_java_list_method())?,
                class_name_jstring,
                array_ptr,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(jni_env, ())?;

            let native_invocation_global_instance = jni_utils::create_global_ref_from_local_ref(native_invocation_instance, jni_env)?;
            // Prevent memory leaks from the created local references
            for inv_arg_jobject in inv_arg_jobjects {
                jni_utils::delete_java_ref(jni_env, inv_arg_jobject);
            }
            jni_utils::delete_java_ref(jni_env, array_ptr);
            jni_utils::delete_java_ref(jni_env, class_name_jstring);

            // Create and return the Instance
            Self::do_return(jni_env, Instance {
                jinstance: native_invocation_global_instance,
                class_name: class_name.to_string(),
            })
        }
    }

    /// Invokes the method `method_name` of a created `Instance`, passing an array of `InvocationArg`s. It returns an `Instance` as the result of the invocation.
    pub fn invoke(&self, instance: &Instance, method_name: &str, inv_args: &[InvocationArg]) -> errors::Result<Instance> {
        debug(&format!("Invoking method {} of class {} using {} arguments", method_name, instance.class_name, inv_args.len()));
        unsafe {
            // First argument: create a jstring to pass as argument for the method_name
            let method_name_jstring: jstring = jni_utils::global_jobject_from_str(&method_name, self.jni_env)?;

            // Rest of the arguments: Create a new objectarray of class InvocationArg
            let size = inv_args.len() as i32;
            let array_ptr = {
                let j = (opt_to_res(cache::get_jni_new_object_array())?)(
                    self.jni_env,
                    size,
                    opt_to_res(cache::get_invocation_arg_class())?,
                    ptr::null_mut(),
                );
                jni_utils::create_global_ref_from_local_ref(j, self.jni_env)?
            };
            let mut inv_arg_jobjects: Vec<jobject> = Vec::new();

            // Rest of the arguments: populate the array
            for i in 0..size {
                // Create an InvocationArg Java Object
                let inv_arg_java = inv_args[i as usize].as_java_ptr(self.jni_env)?;
                // Set it in the array
                (opt_to_res(cache::get_jni_set_object_array_element())?)(
                    self.jni_env,
                    array_ptr,
                    i,
                    inv_arg_java,
                );
                inv_arg_jobjects.push(inv_arg_java);
            }

            // Call the method of the instance
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_object_method())?)(
                self.jni_env,
                instance.jinstance,
                opt_to_res(cache::get_invoke_method())?,
                method_name_jstring,
                array_ptr,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            let native_invocation_global_instance = jni_utils::create_global_ref_from_local_ref(native_invocation_instance, self.jni_env)?;
            // Prevent memory leaks from the created local references
            for inv_arg_jobject in inv_arg_jobjects {
                jni_utils::delete_java_ref(self.jni_env, inv_arg_jobject);
            }
            jni_utils::delete_java_ref(self.jni_env, array_ptr);
            jni_utils::delete_java_ref(self.jni_env, method_name_jstring);

            // Create and return the Instance
            Self::do_return(self.jni_env, Instance {
                jinstance: native_invocation_global_instance,
                class_name: cache::UNKNOWN_FOR_RUST.to_string(),
            })
        }
    }

    /// Retrieves the field `field_name` of a created `Instance`.
    pub fn field(&self, instance: &Instance, field_name: &str) -> errors::Result<Instance> {
        debug(&format!("Retrieving field {} of class {}", field_name, instance.class_name));
        unsafe {
            // First argument: create a jstring to pass as argument for the field_name
            let field_name_jstring: jstring = jni_utils::global_jobject_from_str(&field_name, self.jni_env)?;

            // Call the method of the instance
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_object_method())?)(
                self.jni_env,
                instance.jinstance,
                opt_to_res(cache::get_field_method())?,
                field_name_jstring,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            let native_invocation_global_instance = jni_utils::create_global_ref_from_local_ref(native_invocation_instance, self.jni_env)?;
            // Prevent memory leaks from the created local references
            jni_utils::delete_java_ref(self.jni_env, field_name_jstring);

            // Create and return the Instance
            Self::do_return(self.jni_env, Instance {
                jinstance: native_invocation_global_instance,
                class_name: cache::UNKNOWN_FOR_RUST.to_string(),
            })
        }
    }

    /// Invokes the method `method_name` of a created `Instance`, passing an array of `InvocationArg`s.
    /// It returns a Result of `InstanceReceiver` that may be used to get an underlying `Receiver<Instance>`. The result of the invocation will come via this Receiver.
    pub fn invoke_to_channel(&self, instance: &Instance, method_name: &str, inv_args: &[InvocationArg]) -> errors::Result<InstanceReceiver> {
        debug(&format!("Invoking method {} of class {} using {} arguments. The result of the invocation will come via an InstanceReceiver", method_name, instance.class_name, inv_args.len()));
        unsafe {
            // Create the channel
            let (sender, rx) = channel();
            let tx = Box::new(sender);
            // First argument: the address of the channel Sender
            let raw_ptr = Box::into_raw(tx);
            // Find the address of tx
            let address_string = format!("{:p}", raw_ptr);
            let address = i64::from_str_radix(&address_string[2..], 16).unwrap();

            // Second argument: create a jstring to pass as argument for the method_name
            let method_name_jstring: jstring = jni_utils::global_jobject_from_str(&method_name, self.jni_env)?;

            // Rest of the arguments: Create a new objectarray of class InvocationArg
            let size = inv_args.len() as i32;
            let array_ptr = {
                let j = (opt_to_res(cache::get_jni_new_object_array())?)(
                    self.jni_env,
                    size,
                    opt_to_res(cache::get_invocation_arg_class())?,
                    ptr::null_mut(),
                );
                jni_utils::create_global_ref_from_local_ref(j, self.jni_env)?
            };
            let mut inv_arg_jobjects: Vec<jobject> = Vec::new();

            // Rest of the arguments: populate the array
            for i in 0..size {
                // Create an InvocationArg Java Object
                let inv_arg_java = inv_args[i as usize].as_java_ptr(self.jni_env)?;
                // Set it in the array
                (opt_to_res(cache::get_jni_set_object_array_element())?)(
                    self.jni_env,
                    array_ptr,
                    i,
                    inv_arg_java,
                );
                inv_arg_jobjects.push(inv_arg_java);
            }

            // Call the method of the instance
            let _ = (opt_to_res(cache::get_jni_call_void_method())?)(
                self.jni_env,
                instance.jinstance,
                opt_to_res(cache::get_invoke_to_channel_method())?,
                address,
                method_name_jstring,
                array_ptr,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            // Prevent memory leaks from the created local references
            for inv_arg_jobject in inv_arg_jobjects {
                jni_utils::delete_java_ref(self.jni_env, inv_arg_jobject);
            }
            jni_utils::delete_java_ref(self.jni_env, array_ptr);
            jni_utils::delete_java_ref(self.jni_env, method_name_jstring);

            // Create and return the Instance
            Self::do_return(self.jni_env, InstanceReceiver::new(rx, address))
        }
    }

    pub fn init_callback_channel(&self, instance: &Instance) -> errors::Result<InstanceReceiver> {
        debug(&format!("Initializing callback channel"));
        unsafe {
            // Create the channel
            let (sender, rx) = channel();
            let tx = Box::new(sender);
            // First argument: the address of the channel Sender
            let raw_ptr = Box::into_raw(tx);
            // Find the address of tx
            let address_string = format!("{:p}", raw_ptr);
            let address = i64::from_str_radix(&address_string[2..], 16).unwrap();

            // Call the method of the instance
            let _ = (opt_to_res(cache::get_jni_call_void_method())?)(
                self.jni_env,
                instance.jinstance,
                opt_to_res(cache::get_init_callback_channel_method())?,
                address,
            );

            // Create and return the Instance
            Self::do_return(self.jni_env, InstanceReceiver::new(rx, address))
        }
    }

    /// Invokes the static method `method_name` of the class `class_name`, passing an array of `InvocationArg`s. It returns an `Instance` as the result of the invocation.
    pub fn invoke_static(&self, class_name: &str, method_name: &str, inv_args: &[InvocationArg]) -> errors::Result<Instance> {
        debug(&format!("Invoking static method {} of class {} using {} arguments", method_name, class_name, inv_args.len()));
        unsafe {
            // Factory invocation - first argument: create a jstring to pass as argument for the class_name
            let class_name_jstring: jstring = jni_utils::global_jobject_from_str(&class_name, self.jni_env)?;

            // Call the method of the factory that creates a NativeInvocation for static calls to methods of class `class_name`.
            // This returns a NativeInvocation that acts like a proxy to the Java world.
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                self.jni_env,
                opt_to_res(cache::get_factory_class())?,
                opt_to_res(cache::get_factory_create_for_static_method())?,
                class_name_jstring,
            );

            // First argument: create a jstring to pass as argument for the method_name
            let method_name_jstring: jstring = jni_utils::global_jobject_from_str(&method_name, self.jni_env)?;

            // Rest of the arguments: Create a new objectarray of class InvocationArg
            let size = inv_args.len() as i32;
            let array_ptr = {
                let j = (opt_to_res(cache::get_jni_new_object_array())?)(
                    self.jni_env,
                    size,
                    opt_to_res(cache::get_invocation_arg_class())?,
                    ptr::null_mut(),
                );
                jni_utils::create_global_ref_from_local_ref(j, self.jni_env)?
            };
            let mut inv_arg_jobjects: Vec<jobject> = Vec::new();
            // Rest of the arguments: populate the array
            for i in 0..size {
                // Create an InvocationArg Java Object
                let inv_arg_java = inv_args[i as usize].as_java_ptr(self.jni_env)?;
                // Set it in the array
                (opt_to_res(cache::get_jni_set_object_array_element())?)(
                    self.jni_env,
                    array_ptr,
                    i,
                    inv_arg_java,
                );
                inv_arg_jobjects.push(inv_arg_java);
            }
            // Call the method of the instance
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_object_method())?)(
                self.jni_env,
                native_invocation_instance,
                opt_to_res(cache::get_invoke_static_method())?,
                method_name_jstring,
                array_ptr,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            // Prevent memory leaks from the created local references
            for inv_arg_jobject in inv_arg_jobjects {
                jni_utils::delete_java_ref(self.jni_env, inv_arg_jobject);
            }
            jni_utils::delete_java_ref(self.jni_env, array_ptr);
            jni_utils::delete_java_ref(self.jni_env, method_name_jstring);

            // Create and return the Instance. The Instance::from transforms the passed instance to a global one. No need to transform it here as well.
            Self::do_return(self.jni_env, Instance::from(native_invocation_instance)?)
        }
    }

    /// Creates a clone of the provided Instance
    pub fn clone_instance(&self, instance: &Instance) -> errors::Result<Instance> {
        unsafe {
            // Call the clone method
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                self.jni_env,
                opt_to_res(cache::get_class_to_invoke_clone_and_cast())?,
                opt_to_res(cache::get_clone_static_method())?,
                instance.jinstance,
            );

            // Create and return the Instance
            Self::do_return(self.jni_env, Instance::from(native_invocation_instance)?)
        }
    }

    /// Invokes the static method `method_name` of the class `class_name`, passing an array of `InvocationArg`s. It returns an `Instance` as the result of the invocation.
    pub fn cast(&self, from_instance: &Instance, to_class: &str) -> errors::Result<Instance> {
        debug(&format!("Casting to class {}", to_class));
        unsafe {
            // First argument is the jobject that is inside the from_instance
            // Second argument: create a jstring to pass as argument for the to_class
            let to_class_jstring: jstring = jni_utils::global_jobject_from_str(&to_class, self.jni_env)?;

            // Call the cast method
            let native_invocation_instance = (opt_to_res(cache::get_jni_call_static_object_method())?)(
                self.jni_env,
                opt_to_res(cache::get_class_to_invoke_clone_and_cast())?,
                opt_to_res(cache::get_cast_static_method())?,
                from_instance.jinstance,
                to_class_jstring,
            );

            // Check for exceptions before creating the globalref
            Self::do_return(self.jni_env, ())?;

            // Prevent memory leaks from the created local references
            jni_utils::delete_java_ref(self.jni_env, to_class_jstring);

            // Create and return the Instance
            Self::do_return(self.jni_env, Instance::from(native_invocation_instance)?)
        }
    }

    /// Returns the Rust representation of the provided instance
    pub fn to_rust<T>(&self, instance: Instance) -> errors::Result<T> where T: DeserializeOwned {
        unsafe {
            debug("Invoking the getJson method");
            // Call the getJson method. This returns a localref
            let json_instance = (opt_to_res(cache::get_jni_call_object_method())?)(
                self.jni_env,
                instance.jinstance,
                opt_to_res(cache::get_get_json_method())?,
            );
            let _ = Self::do_return(self.jni_env, "")?;
            debug("Transforming jstring to rust String");
            let global_json_instance = jni_utils::create_global_ref_from_local_ref(json_instance, self.jni_env)?;
            let json = jni_utils::jstring_to_rust_string(&self, global_json_instance as jstring)?;
            jni_utils::delete_java_ref(self.jni_env, global_json_instance);
            Self::do_return(self.jni_env, serde_json::from_str(&json)?)
        }
    }

    /// Deploys a maven artifact in the default j4rs jars location.
    ///
    /// This is useful for build scripts that need jars for the runtime that can be downloaded from Maven.
    ///
    /// The function deploys __only__ the specified artifact, not its transitive dependencies.
    #[deprecated(since = "0.7.0", note = "please use `deploy_artifact` instead")]
    pub fn deploy_maven(&self, artifact: MavenArtifact) -> errors::Result<()> {
        let instance = self.create_instance(
            "org.astonbitecode.j4rs.api.deploy.SimpleMavenDeployer",
            &vec![InvocationArg::try_from(artifact.base)?])?;

        let _ = self.invoke(
            &instance,
            "deploy",
            &vec![
                InvocationArg::try_from(artifact.group)?,
                InvocationArg::try_from(artifact.id)?,
                InvocationArg::try_from(artifact.version)?,
                InvocationArg::try_from(artifact.qualifier)?])?;
        Ok(())
    }

    /// Deploys an artifact in the default j4rs jars location.
    ///
    /// This is useful for build scripts that need jars for the runtime that can be downloaded from e.g. Maven.
    ///
    /// The function deploys __only__ the specified artifact, not its transitive dependencies.
    pub fn deploy_artifact<T: Any + JavaArtifact>(&self, artifact: &T) -> errors::Result<()> {
        let artifact = artifact as &dyn Any;
        if let Some(maven_artifact) = artifact.downcast_ref::<MavenArtifact>() {
            for repo in get_maven_settings().repos.into_iter() {
                let instance = self.create_instance(
                    "org.astonbitecode.j4rs.api.deploy.SimpleMavenDeployer",
                    &vec![
                        InvocationArg::try_from(repo.uri)?,
                        InvocationArg::try_from(&maven_artifact.base)?])?;

                let res = self.invoke(
                    &instance,
                    "deploy",
                    &vec![
                        InvocationArg::try_from(&maven_artifact.group)?,
                        InvocationArg::try_from(&maven_artifact.id)?,
                        InvocationArg::try_from(&maven_artifact.version)?,
                        InvocationArg::try_from(&maven_artifact.qualifier)?]);

                if res.is_ok() {
                    break;
                }
            }

            Ok(())
        } else if let Some(local_jar_artifact) = artifact.downcast_ref::<LocalJarArtifact>() {
            let instance = self.create_instance(
                "org.astonbitecode.j4rs.api.deploy.FileSystemDeployer",
                &vec![InvocationArg::try_from(&local_jar_artifact.base)?])?;

            let _ = self.invoke(
                &instance,
                "deploy",
                &vec![InvocationArg::try_from(&local_jar_artifact.path)?])?;
            Ok(())
        } else {
            Err(J4RsError::GeneralError(format!("Don't know how to deploy artifacts of {:?}", artifact.type_id())))
        }
    }

    /// Copies the jassets default directory and the j4rs dynamic library under the specified location.
    /// This is useful for cases when `with_base_path` method is used when building a Jvm with the JvmBuilder.
    /// Build scripts should use this method.
    pub fn copy_j4rs_libs_under(path: &str) -> errors::Result<()> {
        let mut pb = PathBuf::from(path);
        pb.push("deps");
        fs::create_dir_all(&pb)?;

        let default_jassets_path_buf = utils::default_jassets_path()?;
        let default_jassets_path_string = default_jassets_path_buf.to_str().unwrap().to_owned();

        // Copy the jassets
        let ref mut options = fs_extra::dir::CopyOptions::new();
        options.overwrite = true;
        let _ = fs_extra::copy_items(vec![default_jassets_path_string].as_ref(), path, options)?;

        // Copy the dynamic libraries
        let dynlibs: Vec<String> = utils::find_j4rs_dynamic_libraries_paths()?;

        let _ = fs_extra::copy_items(&dynlibs, &pb, options)?;

        Ok(())
    }

    /// Initiates a chain of operations on Instances.
    pub fn chain(&self, instance: Instance) -> ChainableInstance {
        ChainableInstance::new(instance, &self)
    }

    pub(crate) fn do_return<T>(jni_env: *mut JNIEnv, to_return: T) -> errors::Result<T> {
        unsafe {
            if (opt_to_res(cache::get_jni_exception_check())?)(jni_env) == JNI_TRUE {
                (opt_to_res(cache::get_jni_exception_describe())?)(jni_env);
                (opt_to_res(cache::get_jni_exception_clear())?)(jni_env);
                Err(errors::J4RsError::JavaError("An Exception was thrown by Java... Please check the logs or the console.".to_string()))
            } else {
                Ok(to_return)
            }
        }
    }

    // Retrieves a JNIEnv in the case that a JVM is already created even from another thread.
    fn get_created_vm() -> Option<*mut JNIEnv> {
        unsafe {
            // Get the number of the already created VMs. This is most probably 1, but we retrieve the number just in case...
            let mut created_vms_size: jsize = 0;
            tweaks::get_created_java_vms(&mut Vec::new(), 0, &mut created_vms_size);

            if created_vms_size == 0 {
                None
            } else {
                debug(&format!("Retrieving the first of {} created JVMs", created_vms_size));
                // Get the created VM
                let mut buffer: Vec<*mut JavaVM> = Vec::new();
                for _ in 0..created_vms_size { buffer.push(ptr::null_mut()); }

                let retjint = tweaks::get_created_java_vms(&mut buffer, created_vms_size, &mut created_vms_size);
                if retjint == JNI_OK {
                    match (**buffer[0]).AttachCurrentThread {
                        Some(act) => {
                            let mut jni_environment: *mut JNIEnv = ptr::null_mut();
                            (act)(
                                buffer[0],
                                (&mut jni_environment as *mut *mut JNIEnv) as *mut *mut c_void,
                                ptr::null_mut(),
                            );
                            Some(jni_environment)
                        }
                        None => {
                            error("Cannot attach the thread to the JVM");
                            None
                        }
                    }
                } else {
                    error(&format!("Error while retrieving the created JVMs: {}", retjint));
                    None
                }
            }
        }
    }

    fn detach_current_thread(&self) {
        unsafe {
            // Get the number of the already created VMs. This is most probably 1, but we retrieve the number just in case...
            let mut created_vms_size: jsize = 0;
            tweaks::get_created_java_vms(&mut Vec::new(), 0, &mut created_vms_size);

            if created_vms_size > 0 {
                // Get the created VM
                let mut buffer: Vec<*mut JavaVM> = Vec::new();
                for _ in 0..created_vms_size { buffer.push(ptr::null_mut()); }

                let retjint = tweaks::get_created_java_vms(&mut buffer, created_vms_size, &mut created_vms_size);
                if retjint == JNI_OK {
                    match (**buffer[0]).DetachCurrentThread {
                        Some(dct) => {
                            (dct)(buffer[0]);
                        }
                        None => {
                            warn("Cannot detach the thread from the JVM");
                        }
                    }
                } else {
                    warn(&format!("Error while retrieving the created JVMs: {}", retjint));
                }
            }
        }
    }
}

impl Drop for Jvm {
    fn drop(&mut self) {
        if cache::remove_active_jvm() <= 0 {
            if self.detach_thread_on_drop {
                self.detach_current_thread();
            }
            cache::set_thread_local_env(None);
        }
    }
}

/// A builder for Jvm
pub struct JvmBuilder<'a> {
    classpath_entries: Vec<ClasspathEntry<'a>>,
    java_opts: Vec<JavaOpt<'a>>,
    no_implicit_classpath: bool,
    detach_thread_on_drop: bool,
    lib_name_opt: Option<String>,
    skip_setting_native_lib: bool,
    base_path: Option<String>,
    maven_settings: MavenSettings,
}

impl<'a> JvmBuilder<'a> {
    /// Creates a new JvmBuilder.
    pub fn new<'b>() -> JvmBuilder<'b> {
        JvmBuilder {
            classpath_entries: Vec::new(),
            java_opts: Vec::new(),
            no_implicit_classpath: false,
            detach_thread_on_drop: true,
            lib_name_opt: None,
            skip_setting_native_lib: false,
            base_path: None,
            maven_settings: MavenSettings::default(),
        }
    }

    /// Adds a classpath entry.
    pub fn classpath_entry(&'a mut self, cp_entry: ClasspathEntry<'a>) -> &'a mut JvmBuilder {
        self.classpath_entries.push(cp_entry);
        self
    }

    /// Adds classpath entries.
    pub fn classpath_entries(&'a mut self, cp_entries: Vec<ClasspathEntry<'a>>) -> &'a mut JvmBuilder {
        for cp_entry in cp_entries {
            self.classpath_entries.push(cp_entry);
        }
        self
    }

    /// Adds a Java option.
    pub fn java_opt(&'a mut self, opt: JavaOpt<'a>) -> &'a mut JvmBuilder {
        self.java_opts.push(opt);
        self
    }

    /// Adds Java options.
    pub fn java_opts(&'a mut self, opts: Vec<JavaOpt<'a>>) -> &'a mut JvmBuilder {
        for opt in opts {
            self.java_opts.push(opt);
        }
        self
    }

    /// By default, the created `Jvm`s include an implicit classpath entry that includes the j4rs jar.
    /// When `with_no_implicit_classpath()` is called, this classpath will not be added to the Jvm.
    pub fn with_no_implicit_classpath(&'a mut self) -> &'a mut JvmBuilder {
        self.no_implicit_classpath = true;
        self
    }

    /// When a Jvm goes out of scope and is being dropped, its current thread is being detached from the Java VM.
    /// A Jvm that is created with `detach_thread_on_drop(false)` will not detach the thread when being dropped.
    ///
    /// This is useful when in the Java world a native method is called and in the native code someone needs to create a j4rs Jvm.
    /// If that Jvm detaches its current thread when being dropped, there will be problems for the Java world code to continue executing.
    pub fn detach_thread_on_drop(&'a mut self, detach_thread_on_drop: bool) -> &'a mut JvmBuilder {
        self.detach_thread_on_drop = detach_thread_on_drop;
        self
    }

    /// In the case that the j4rs is statically linked to some other library, the Java world (j4rs.jar) needs to load that
    /// library instead of the default one.
    ///
    /// This function defines the native library name to load.
    pub fn with_native_lib_name(&'a mut self, lib_name: &str) -> &'a mut JvmBuilder {
        self.lib_name_opt = Some(lib_name.to_string());
        self
    }

    /// Instructs the builder not to instruct the Java world j4rs code not to load the native library.
    /// (most probably because it is already loaded)
    pub fn skip_setting_native_lib(&'a mut self) -> &'a mut JvmBuilder {
        self.skip_setting_native_lib = true;
        self
    }

    /// Defines the location of the jassets and deps directory.
    /// The jassets contains the j4rs jar and the deps the j4rs dynamic library.
    pub fn with_base_path(&'a mut self, base_path: &str) -> &'a mut JvmBuilder {
        self.base_path = Some(base_path.to_string());
        self
    }

    /// Defines the maven settings to use for provisioning maven artifacts.
    pub fn with_maven_settings(&'a mut self, maven_settings: MavenSettings) -> &'a mut JvmBuilder {
        self.maven_settings = maven_settings;
        self
    }

    /// Creates a Jvm
    pub fn build(&self) -> errors::Result<Jvm> {
        let classpath = if self.no_implicit_classpath {
            self.classpath_entries
                .iter()
                .fold(
                    ".".to_string(),
                    |all, elem| {
                        format!("{}{}{}", all, utils::classpath_sep(), elem.to_string())
                    })
        } else {
            // The default classpath contains all the jars in the jassets directory
            let jassets_path = match &self.base_path {
                Some(base_path_string) => {
                    let mut pb = PathBuf::from(base_path_string);
                    pb.push("jassets");
                    let mut global_jassets_path_opt = cache::JASSETS_PATH.lock()?;
                    *global_jassets_path_opt = Some(pb.clone());
                    pb
                }
                None => utils::default_jassets_path()?,
            };
            let all_jars = get_dir_content(&jassets_path)?.files;
            // This is the j4rs jar that should be included in the classpath
            let j4rs_jar_to_use = format!("j4rs-{}-jar-with-dependencies.jar", j4rs_version());
            // Filter out possible incorrect jars of j4rs
            let filtered_jars: Vec<String> = all_jars.into_iter()
                .filter(|jar| {
                    !jar.contains("j4rs-") || jar.ends_with(&j4rs_jar_to_use)
                })
                .collect();
            let cp_string = filtered_jars.join(utils::classpath_sep());

            let default_class_path = format!("-Djava.class.path={}", cp_string);

            self.classpath_entries
                .iter()
                .fold(
                    default_class_path,
                    |all, elem| {
                        format!("{}{}{}", all, utils::classpath_sep(), elem.to_string())
                    })
        };
        info(&format!("Setting classpath to {}", classpath));

        // Populate the JVM Options
        let mut jvm_options = if self.no_implicit_classpath {
            vec![classpath]
        } else {
            let default_library_path = utils::java_library_path()?;
            info(&format!("Setting library path to {}", default_library_path));
            vec![classpath, default_library_path]
        };
        self.java_opts.clone().into_iter().for_each(|opt| jvm_options.push(opt.to_string()));

        // Pass to the Java world the name of the j4rs library.
        let lib_name_opt = if self.lib_name_opt.is_none() && !self.skip_setting_native_lib {
            let deps_dir = utils::deps_dir()?;
            let found_libs: Vec<String> = if Path::new(&deps_dir).exists() {
                utils::find_j4rs_dynamic_libraries_names()?
            } else {
                // If deps dir is not found, fallback to default naming in order for the library to be searched in the default
                // library locations of the system.
                let default_lib_name = if cfg!(windows) {
                    "l4rs.dll".to_string()
                } else {
                    "libj4rs.so".to_string()
                };
                info(&format!("Deps directory not found. Setting the library name to search to default: {}", default_lib_name));
                vec![default_lib_name]
            };

            let lib_name_opt = if found_libs.len() > 0 {
                let a_lib = found_libs[0].clone().replace("lib", "");

                let dot_splitted: Vec<&str> = a_lib.split(".").collect();
                let name = dot_splitted[0].to_string();
                info(&format!("Passing to the Java world the name of the library to load: {}", name));
                Some(name)
            } else {
                None
            };
            lib_name_opt
        } else if self.lib_name_opt.is_some() && !self.skip_setting_native_lib {
            let name = self.lib_name_opt.clone();
            info(&format!("Passing to the Java world the name of the library to load: {}", name.as_ref().unwrap()));
            name
        } else {
            None
        };

        provisioning::set_maven_settings(&self.maven_settings);

        Jvm::new(&jvm_options, lib_name_opt)
            .and_then(|mut jvm| {
                if !self.detach_thread_on_drop {
                    jvm.detach_thread_on_drop(false);
                }
                Ok(jvm)
            })
    }

    /// Creates a Jvm, similar with an already created j4rs Jvm.
    ///
    /// _Note: The already created Jvm is a j4rs Jvm, not a Java VM._
    pub fn already_initialized() -> errors::Result<Jvm> {
        Jvm::new(&Vec::new(), None)
    }
}

/// Struct that carries an argument that is used for method invocations in Java.
#[derive(Serialize)]
pub enum InvocationArg {
    /// An arg that is created in the Java world.
    Java {
        instance: Instance,
        class_name: String,
        serialized: bool,
    },
    /// A serialized arg that is created in the Rust world.
    Rust {
        json: String,
        class_name: String,
        serialized: bool,
    },
    /// An non-serialized arg created in the Rust world, that contains a Java instance.
    ///
    /// The instance is a Basic Java type, like Integer, Float, String etc.
    RustBasic {
        instance: Instance,
        class_name: String,
        serialized: bool,
    },
}

impl InvocationArg {
    /// Creates a InvocationArg::Rust.
    /// This is default for the Args that are created from the Rust code.
    pub fn new<T>(arg: &T, class_name: &str) -> InvocationArg
        where T: Serialize + Any
    {
        Self::new_2(
            arg,
            class_name,
            cache::get_thread_local_env().expect("Could not find the jni_env in the local cache. Please make sure that you created a Jvm before using Jvm::new"))
            .expect("Could not create the InvocationArg. Please see the logs/console for more details.")
    }

    pub fn new_2<T>(arg: &T, class_name: &str, jni_env: *mut JNIEnv) -> errors::Result<InvocationArg>
        where T: Serialize + Any
    {
        let arg_any = arg as &dyn Any;
        if let Some(a) = arg_any.downcast_ref::<String>() {
            Ok(InvocationArg::RustBasic {
                instance: Instance::new(jni_utils::global_jobject_from_str(a, jni_env)?, class_name),
                class_name: class_name.to_string(),
                serialized: false,
            })
        } else if let Some(a) = arg_any.downcast_ref::<i8>() {
            Ok(InvocationArg::RustBasic {
                instance: Instance::new(jni_utils::global_jobject_from_i8(a, jni_env)?, class_name),
                class_name: class_name.to_string(),
                serialized: false,
            })
        } else if let Some(a) = arg_any.downcast_ref::<i16>() {
            Ok(InvocationArg::RustBasic {
                instance: Instance::new(jni_utils::global_jobject_from_i16(a, jni_env)?, class_name),
                class_name: class_name.to_string(),
                serialized: false,
            })
        } else if let Some(a) = arg_any.downcast_ref::<i32>() {
            Ok(InvocationArg::RustBasic {
                instance: Instance::new(jni_utils::global_jobject_from_i32(a, jni_env)?, class_name),
                class_name: class_name.to_string(),
                serialized: false,
            })
        } else if let Some(a) = arg_any.downcast_ref::<i64>() {
            Ok(InvocationArg::RustBasic {
                instance: Instance::new(jni_utils::global_jobject_from_i64(a, jni_env)?, class_name),
                class_name: class_name.to_string(),
                serialized: false,
            })
        } else {
            let json = serde_json::to_string(arg)?;
            Ok(InvocationArg::Rust {
                json: json,
                class_name: class_name.to_string(),
                serialized: true,
            })
        }
    }

    fn make_primitive(&mut self) -> errors::Result<()> {
        match utils::primitive_of(self) {
            Some(primitive_repr) => {
                match self {
                    &mut InvocationArg::Java { instance: _, ref mut class_name, serialized: _ } => *class_name = primitive_repr,
                    &mut InvocationArg::Rust { json: _, ref mut class_name, serialized: _ } => *class_name = primitive_repr,
                    &mut InvocationArg::RustBasic { instance: _, ref mut class_name, serialized: _ } => *class_name = primitive_repr,
                };
                Ok(())
            }
            None => Err(errors::J4RsError::JavaError(format!("Cannot transform to primitive: {}", utils::get_class_name(&self))))
        }
    }

    /// Consumes this InvocationArg and transforms it to an InvocationArg that contains a Java primitive, leveraging Java's autoboxing.
    ///
    /// This action can be done by calling `Jvm::cast` of Instances as well (e.g.: jvm.cast(&instance, "int"))
    /// but calling `into_primitive` is faster, as it does not involve JNI calls.
    pub fn into_primitive(self) -> errors::Result<InvocationArg> {
        let mut ia = self;
        ia.make_primitive()?;
        Ok(ia)
    }

    /// Creates a `jobject` from this InvocationArg.
    pub fn as_java_ptr(&self, jni_env: *mut JNIEnv) -> errors::Result<jobject> {
        match self {
            _s @ &InvocationArg::Java { .. } => jni_utils::invocation_arg_jobject_from_java(&self, jni_env),
            _s @ &InvocationArg::Rust { .. } => jni_utils::invocation_arg_jobject_from_rust_serialized(&self, jni_env),
            _s @ &InvocationArg::RustBasic { .. } => jni_utils::invocation_arg_jobject_from_rust_basic(&self, jni_env),
        }
    }

    /// Consumes this invocation arg and returns its Instance
    pub fn instance(self) -> errors::Result<Instance> {
        match self {
            InvocationArg::Java { instance: i, .. } => Ok(i),
            InvocationArg::RustBasic { .. } => Err(errors::J4RsError::RustError(format!("Invalid operation: Cannot get the instance of an InvocationArg::RustBasic"))),
            InvocationArg::Rust { .. } => Err(errors::J4RsError::RustError(format!("Cannot get the instance from an InvocationArg::Rust"))),
        }
    }
}

impl From<Instance> for InvocationArg {
    fn from(instance: Instance) -> InvocationArg {
        let class_name = instance.class_name.to_owned();

        InvocationArg::Java {
            instance: instance,
            class_name: class_name,
            serialized: false,
        }
    }
}

impl TryFrom<String> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: String) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.String", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [String]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [String]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl<'a> TryFrom<&'a str> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a str) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg.to_string(), "java.lang.String", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [&'a str]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [&'a str]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.iter().map(|&elem| InvocationArg::try_from(elem)).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<bool> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: bool) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Boolean", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [bool]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [bool]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<i8> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: i8) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Byte", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [i8]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [i8]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<char> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: char) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Character", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [char]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [char]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<i16> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: i16) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Short", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [i16]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [i16]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<i32> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: i32) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Integer", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [i32]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [i32]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<i64> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: i64) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Long", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [i64]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [i64]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<f32> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: f32) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Float", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [f32]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [f32]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<f64> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: f64) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "java.lang.Double", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a [f64]> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(vec: &'a [f64]) -> errors::Result<InvocationArg> {
        let args: errors::Result<Vec<InvocationArg>> = vec.into_iter().map(|elem| InvocationArg::try_from(elem.clone())).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl<'a, T: 'static> TryFrom<(&'a [T], &'a str)> for InvocationArg where T: Serialize {
    type Error = errors::J4RsError;
    fn try_from(vec: (&'a [T], &'a str)) -> errors::Result<InvocationArg> {
        let (vec, elements_class_name) = vec;
        let jni_env = cache::get_thread_local_env()?;
        let args: errors::Result<Vec<InvocationArg>> = vec.iter().map(|elem| InvocationArg::new_2(elem, elements_class_name, jni_env)).collect();
        let res = Jvm::do_create_java_list(cache::get_thread_local_env()?, cache::J4RS_ARRAY, &args?);
        Ok(InvocationArg::from(res?))
    }
}

impl TryFrom<()> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: ()) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(&arg, "void", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a String> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a String) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.String", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a bool,> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a bool) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Boolean", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a i8> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a i8) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Byte", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a char> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a char) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Character", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a i16> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a i16) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Short", cache::get_thread_local_env()?)
    }
}

impl<'a, 'b> TryFrom<&'a i32> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a i32) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Integer", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a i64> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a i64) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Long", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a f32> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a f32) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Float", cache::get_thread_local_env()?)
    }
}

impl<'a> TryFrom<&'a f64> for InvocationArg {
    type Error = errors::J4RsError;
    fn try_from(arg: &'a f64) -> errors::Result<InvocationArg> {
        InvocationArg::new_2(arg, "java.lang.Double", cache::get_thread_local_env()?)
    }
}

/// A receiver for Java Instances.
///
/// It keeps a channel Receiver to get callback Instances from the Java world
/// and the address of a Box<Sender<Instance>> Box in the heap. This Box is used by Java to communicate
/// asynchronously Instances to Rust.
///
/// On Drop, the InstanceReceiver removes the Box from the heap.
pub struct InstanceReceiver {
    rx: Box<Receiver<Instance>>,
    tx_address: i64,
}

impl InstanceReceiver {
    fn new(rx: Receiver<Instance>, tx_address: i64) -> InstanceReceiver {
        InstanceReceiver {
            rx: Box::new(rx),
            tx_address,
        }
    }

    pub fn rx(&self) -> &Receiver<Instance> {
        &self.rx
    }
}

impl Drop for InstanceReceiver {
    fn drop(&mut self) {
        debug("Dropping an InstanceReceiver");
        let p = self.tx_address as *mut Sender<Instance>;
        unsafe {
            let tx = Box::from_raw(p);
            mem::drop(tx);
        }
    }
}

/// A Java instance
#[derive(Serialize)]
pub struct Instance {
    /// The name of the class of this instance
    class_name: String,
    /// The JNI jobject that manipulates this instance.
    ///
    /// This object is an instance of `org/astonbitecode/j4rs/api/NativeInvocation`
    #[serde(skip)]
    pub(crate) jinstance: jobject,
}

impl Instance {
    pub(crate) fn new(obj: jobject, classname: &str) -> Instance {
        Instance {
            jinstance: obj,
            class_name: classname.to_string(),
        }
    }

    /// Returns the class name of this instance
    pub fn class_name(&self) -> &str {
        self.class_name.as_ref()
    }

    /// Consumes the Instance and returns its jobject
    pub fn java_object(self) -> jobject {
        self.jinstance
    }

    pub fn from(obj: jobject) -> errors::Result<Instance> {
        let _jvm = cache::get_thread_local_env().map_err(|_| {
            Jvm::attach_thread()
        });

        let global = jni_utils::create_global_ref_from_local_ref(obj, cache::get_thread_local_env()?)?;
        Ok(Instance {
            jinstance: global,
            class_name: cache::UNKNOWN_FOR_RUST.to_string(),
        })
    }

    /// Creates a weak reference of this Instance.
    fn _weak_ref(&self) -> errors::Result<Instance> {
        Ok(Instance {
            class_name: self.class_name.clone(),
            jinstance: jni_utils::_create_weak_global_ref_from_global_ref(self.jinstance.clone(), cache::get_thread_local_env()?)?,
        })
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        debug(&format!("Dropping an instance of {}", self.class_name));
        if let Some(j_env) = cache::get_thread_local_env_opt() {
            jni_utils::delete_java_ref(j_env, self.jinstance);
        }
    }
}

unsafe impl Send for Instance {}

/// Allows chained Jvm calls to created Instances
pub struct ChainableInstance<'a> {
    instance: Instance,
    jvm: &'a Jvm,
}

impl<'a> ChainableInstance<'a> {
    fn new(instance: Instance, jvm: &'a Jvm) -> ChainableInstance {
        ChainableInstance { instance, jvm }
    }

    pub fn collect(self) -> Instance {
        self.instance
    }

    /// Invokes the method `method_name` of a this `Instance`, passing an array of `InvocationArg`s. It returns an `Instance` as the result of the invocation.
    pub fn invoke(&self, method_name: &str, inv_args: &[InvocationArg]) -> errors::Result<ChainableInstance> {
        let instance = self.jvm.invoke(&self.instance, method_name, inv_args)?;
        Ok(ChainableInstance::new(instance, self.jvm))
    }

    /// Creates a clone of the Instance
    pub fn clone_instance(&self) -> errors::Result<ChainableInstance> {
        let instance = self.jvm.clone_instance(&self.instance)?;
        Ok(ChainableInstance::new(instance, self.jvm))
    }

    /// Invokes the static method `method_name` of the class `class_name`, passing an array of `InvocationArg`s. It returns an `Instance` as the result of the invocation.
    pub fn cast(&self, to_class: &str) -> errors::Result<ChainableInstance> {
        let instance = self.jvm.cast(&self.instance, to_class)?;
        Ok(ChainableInstance::new(instance, self.jvm))
    }

    /// Retrieves the field `field_name` of the `Instance`.
    pub fn field(&self, field_name: &str) -> errors::Result<ChainableInstance> {
        let instance = self.jvm.field(&self.instance, field_name)?;
        Ok(ChainableInstance::new(instance, self.jvm))
    }

    /// Returns the Rust representation of the provided instance
    pub fn to_rust<T>(self) -> errors::Result<T> where T: DeserializeOwned {
        self.jvm.to_rust(self.instance)
    }
}

/// A classpath entry.
#[derive(Debug, Clone)]
pub struct ClasspathEntry<'a> (&'a str);

impl<'a> ClasspathEntry<'a> {
    pub fn new(classpath_entry: &str) -> ClasspathEntry {
        ClasspathEntry(classpath_entry)
    }
}

impl<'a> ToString for ClasspathEntry<'a> {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

/// A Java Option.
#[derive(Debug, Clone)]
pub struct JavaOpt<'a> (&'a str);

impl<'a> JavaOpt<'a> {
    pub fn new(java_opt: &str) -> JavaOpt {
        JavaOpt(java_opt)
    }
}

impl<'a> ToString for JavaOpt<'a> {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

#[cfg(test)]
mod api_unit_tests {
    use serde::Deserialize;
    use serde_json;

    use super::*;

    #[test]
    fn jvm_builder() {
        let res = JvmBuilder::new().build();
        assert!(res.is_ok());
        let one_more_res = JvmBuilder::already_initialized();
        assert!(one_more_res.is_ok());
    }

    #[test]
    fn new_invocation_arg() {
        let _jvm = JvmBuilder::new().build().unwrap();
        let _ = InvocationArg::new(&"something".to_string(), "somethingelse");

        let gr = GuiResponse::ProvidedPassword { password: "passs".to_string(), number: 1 };
        let json = serde_json::to_string(&gr).unwrap();
        println!("{:?}", json);
        let res: Result<GuiResponse, _> = serde_json::from_str(&json);
        println!("{:?}", res);
    }

    #[derive(Serialize, Deserialize, Debug)]
    enum GuiResponse {
        ProvidedPassword { password: String, number: usize }
    }

    #[test]
    fn invocation_arg_try_from_basic_types() {
        let _jvm = JvmBuilder::new().build().unwrap();
        validate_type(InvocationArg::try_from("str").unwrap(), "java.lang.String");
        validate_type(InvocationArg::try_from("str".to_string()).unwrap(), "java.lang.String");
        validate_type(InvocationArg::try_from(true).unwrap(), "java.lang.Boolean");
        validate_type(InvocationArg::try_from(1_i8).unwrap(), "java.lang.Byte");
        validate_type(InvocationArg::try_from('c').unwrap(), "java.lang.Character");
        validate_type(InvocationArg::try_from(1_i16).unwrap(), "java.lang.Short");
        validate_type(InvocationArg::try_from(1_i64).unwrap(), "java.lang.Long");
        validate_type(InvocationArg::try_from(0.1_f32).unwrap(), "java.lang.Float");
        validate_type(InvocationArg::try_from(0.1_f64).unwrap(), "java.lang.Double");
        validate_type(InvocationArg::try_from(()).unwrap(), "void");

        validate_type(InvocationArg::try_from(&"str".to_string()).unwrap(), "java.lang.String");
        validate_type(InvocationArg::try_from(&true).unwrap(), "java.lang.Boolean");
        validate_type(InvocationArg::try_from(&1_i8).unwrap(), "java.lang.Byte");
        validate_type(InvocationArg::try_from(&'c').unwrap(), "java.lang.Character");
        validate_type(InvocationArg::try_from(&1_i16).unwrap(), "java.lang.Short");
        validate_type(InvocationArg::try_from(&1_i64).unwrap(), "java.lang.Long");
        validate_type(InvocationArg::try_from(&0.1_f32).unwrap(), "java.lang.Float");
        validate_type(InvocationArg::try_from(&0.1_f64).unwrap(), "java.lang.Double");
    }

    #[test]
    fn invocation_into_primitive() {
        let _jvm: Jvm = JvmBuilder::new().build().unwrap();
        assert!(InvocationArg::try_from(false).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(1_i8).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(1_i16).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(1_32).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(1_i64).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(0.1_f32).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(0.1_f64).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from('c').unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from(()).unwrap().into_primitive().is_ok());
        assert!(InvocationArg::try_from("string").unwrap().into_primitive().is_err());
    }

    #[test]
    fn test_copy_j4rs_libs_under() {
        let newdir = "./newdir";
        Jvm::copy_j4rs_libs_under(newdir).unwrap();

        let _ = fs_extra::remove_items(&vec![newdir]);
    }

    fn validate_type(ia: InvocationArg, class: &str) {
        let b = match ia {
            _s @ InvocationArg::Java { .. } => false,
            InvocationArg::Rust { class_name, json: _, .. } => {
                class == class_name
            }
            InvocationArg::RustBasic { instance: _, class_name, serialized: _ } => {
                class == class_name
            }
        };
        assert!(b);
    }
}