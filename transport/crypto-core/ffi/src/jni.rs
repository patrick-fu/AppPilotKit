use super::*;
use core::ffi::c_void;
use jni_sys::{
    JNI_ERR, JNI_OK, JNI_VERSION_1_6, JNIEnv, JNINativeMethod, JavaVM, jint, jlong, jobject,
};

const NATIVE_CLASS: &[u8] = b"dev/apppilotkit/transport/NativeTransport\0";

fn outcome_buffer_is_valid(address: *mut u8, capacity: u64) -> bool {
    !address.is_null()
        && capacity >= std::mem::size_of::<OutcomeV1>() as u64
        && address
            .addr()
            .is_multiple_of(std::mem::align_of::<OutcomeV1>())
}

unsafe fn direct_buffer(env: *mut JNIEnv, buffer: jobject) -> Option<(*mut u8, u64)> {
    if env.is_null() || buffer.is_null() {
        return None;
    }
    let functions = unsafe { &**env };
    let functions = unsafe { &functions.v1_6 };
    let address = unsafe { (functions.GetDirectBufferAddress)(env, buffer) }.cast::<u8>();
    let capacity = unsafe { (functions.GetDirectBufferCapacity)(env, buffer) };
    if address.is_null() || capacity < 0 {
        None
    } else {
        Some((address, capacity as u64))
    }
}

unsafe extern "system" fn native_abi_version(_env: *mut JNIEnv, _class: jobject) -> jint {
    apppilotkit_tp_v1_abi_version() as jint
}

unsafe extern "system" fn native_create(
    env: *mut JNIEnv,
    _class: jobject,
    descriptor: jobject,
    descriptor_len: jlong,
    outcome: jobject,
) -> jlong {
    if descriptor_len < 0 {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    }
    let Some((descriptor, capacity)) = (unsafe { direct_buffer(env, descriptor) }) else {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    };
    let Some((outcome, outcome_capacity)) = (unsafe { direct_buffer(env, outcome) }) else {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    };
    if descriptor_len as u64 > capacity || !outcome_buffer_is_valid(outcome, outcome_capacity) {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    }
    let input = CreateInputV1 {
        abi_version: ABI_VERSION,
        struct_size: std::mem::size_of::<CreateInputV1>() as u32,
        descriptor_cbor: descriptor,
        descriptor_len: descriptor_len as u64,
    };
    let mut handle = 0;
    let status =
        unsafe { apppilotkit_tp_v1_create(&input, &mut handle, outcome.cast::<OutcomeV1>()) };
    if status < 0 {
        jlong::from(status)
    } else {
        handle as jlong
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn native_drive(
    env: *mut JNIEnv,
    _class: jobject,
    handle: jlong,
    tag: jint,
    flags: jint,
    stream_id: jlong,
    write_token: jlong,
    bytes: jobject,
    bytes_len: jlong,
    outcome: jobject,
) -> jint {
    if handle <= 0 || tag < 0 || flags < 0 || stream_id < 0 || write_token < 0 || bytes_len < 0 {
        return STATUS_INVALID_ARGUMENT;
    }
    let (bytes, capacity) = if bytes_len == 0 && bytes.is_null() {
        (std::ptr::null_mut(), 0)
    } else {
        let Some(value) = (unsafe { direct_buffer(env, bytes) }) else {
            return STATUS_INVALID_ARGUMENT;
        };
        value
    };
    let Some((outcome, outcome_capacity)) = (unsafe { direct_buffer(env, outcome) }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if bytes_len as u64 > capacity || !outcome_buffer_is_valid(outcome, outcome_capacity) {
        return STATUS_INVALID_ARGUMENT;
    }
    let event = EventV1 {
        abi_version: ABI_VERSION,
        struct_size: std::mem::size_of::<EventV1>() as u32,
        tag: tag as u32,
        flags: flags as u32,
        stream_id: stream_id as u64,
        write_token: write_token as u64,
        bytes,
        bytes_len: bytes_len as u64,
    };
    unsafe { apppilotkit_tp_v1_drive(handle as u64, &event, outcome.cast::<OutcomeV1>()) }
}

unsafe extern "system" fn native_close(
    env: *mut JNIEnv,
    _class: jobject,
    handle: jlong,
    outcome: jobject,
) -> jint {
    if handle < 0 {
        return STATUS_INVALID_ARGUMENT;
    }
    let Some((outcome, capacity)) = (unsafe { direct_buffer(env, outcome) }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if !outcome_buffer_is_valid(outcome, capacity) {
        return STATUS_INVALID_ARGUMENT;
    }
    let mut handle = handle as u64;
    unsafe { apppilotkit_tp_v1_close(&mut handle, outcome.cast::<OutcomeV1>()) }
}

unsafe extern "system" fn native_drop(_env: *mut JNIEnv, _class: jobject, handle: jlong) -> jint {
    if handle < 0 {
        return STATUS_INVALID_ARGUMENT;
    }
    let mut handle = handle as u64;
    unsafe { apppilotkit_tp_v1_drop(&mut handle) }
}

unsafe extern "system" fn native_output_count(
    _env: *mut JNIEnv,
    _class: jobject,
    handle: jlong,
) -> jlong {
    if handle <= 0 {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    }
    let mut count = 0;
    let status = unsafe { apppilotkit_tp_v1_output_count(handle as u64, &mut count) };
    if status < 0 {
        jlong::from(status)
    } else {
        count as jlong
    }
}

unsafe extern "system" fn native_output_len(
    _env: *mut JNIEnv,
    _class: jobject,
    output: jlong,
) -> jlong {
    if output <= 0 {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    }
    let mut length = 0;
    let status = unsafe { apppilotkit_tp_v1_output_len(output as u64, &mut length) };
    if status < 0 {
        jlong::from(status)
    } else {
        length as jlong
    }
}

unsafe extern "system" fn native_output_copy(
    env: *mut JNIEnv,
    _class: jobject,
    output: jlong,
    destination: jobject,
    capacity: jlong,
) -> jlong {
    if output <= 0 || capacity < 0 {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    }
    let Some((destination, buffer_capacity)) = (unsafe { direct_buffer(env, destination) }) else {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    };
    if capacity as u64 > buffer_capacity {
        return jlong::from(STATUS_INVALID_ARGUMENT);
    }
    let mut written = 0;
    let status = unsafe {
        apppilotkit_tp_v1_output_copy(output as u64, destination, capacity as u64, &mut written)
    };
    if status < 0 {
        jlong::from(status)
    } else {
        written as jlong
    }
}

unsafe extern "system" fn native_output_drop(
    _env: *mut JNIEnv,
    _class: jobject,
    output: jlong,
) -> jint {
    if output < 0 {
        return STATUS_INVALID_ARGUMENT;
    }
    let mut output = output as u64;
    unsafe { apppilotkit_tp_v1_output_drop(&mut output) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(vm: *mut JavaVM, _reserved: *mut c_void) -> jint {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if vm.is_null() {
            return JNI_ERR;
        }
        let invoke = unsafe { &**vm };
        let invoke = unsafe { &invoke.v1_4 };
        let mut raw_env: *mut c_void = std::ptr::null_mut();
        if unsafe { (invoke.GetEnv)(vm, &mut raw_env, JNI_VERSION_1_6) } != JNI_OK
            || raw_env.is_null()
        {
            return JNI_ERR;
        }
        let env = raw_env.cast::<JNIEnv>();
        let functions = unsafe { &**env };
        let functions = unsafe { &functions.v1_6 };
        let class = unsafe { (functions.FindClass)(env, NATIVE_CLASS.as_ptr().cast()) };
        if class.is_null() {
            return JNI_ERR;
        }
        let methods = [
            method(b"abiVersion\0", b"()I\0", native_abi_version as *mut c_void),
            method(
                b"create\0",
                b"(Ljava/nio/ByteBuffer;JLjava/nio/ByteBuffer;)J\0",
                native_create as *mut c_void,
            ),
            method(
                b"drive\0",
                b"(JIIJJLjava/nio/ByteBuffer;JLjava/nio/ByteBuffer;)I\0",
                native_drive as *mut c_void,
            ),
            method(
                b"close\0",
                b"(JLjava/nio/ByteBuffer;)I\0",
                native_close as *mut c_void,
            ),
            method(b"drop\0", b"(J)I\0", native_drop as *mut c_void),
            method(
                b"outputCount\0",
                b"(J)J\0",
                native_output_count as *mut c_void,
            ),
            method(b"outputLen\0", b"(J)J\0", native_output_len as *mut c_void),
            method(
                b"outputCopy\0",
                b"(JLjava/nio/ByteBuffer;J)J\0",
                native_output_copy as *mut c_void,
            ),
            method(
                b"outputDrop\0",
                b"(J)I\0",
                native_output_drop as *mut c_void,
            ),
        ];
        if unsafe {
            (functions.RegisterNatives)(env, class, methods.as_ptr(), methods.len() as jint)
        } != JNI_OK
        {
            return JNI_ERR;
        }
        JNI_VERSION_1_6
    }));
    result.unwrap_or(JNI_ERR)
}

fn method(name: &'static [u8], signature: &'static [u8], function: *mut c_void) -> JNINativeMethod {
    JNINativeMethod {
        name: name.as_ptr().cast_mut().cast(),
        signature: signature.as_ptr().cast_mut().cast(),
        fnPtr: function,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jni_sys::{JNINativeInterface_, JNINativeInterface__1_6};
    use std::mem::MaybeUninit;

    struct DirectBuffer {
        bytes: Vec<u8>,
    }

    impl DirectBuffer {
        fn as_jobject(&mut self) -> jobject {
            std::ptr::from_mut(self).cast()
        }
    }

    unsafe extern "system" fn direct_buffer_address(
        _env: *mut JNIEnv,
        buffer: jobject,
    ) -> *mut c_void {
        let buffer = unsafe { &mut *buffer.cast::<DirectBuffer>() };
        buffer.bytes.as_mut_ptr().cast()
    }

    unsafe extern "system" fn direct_buffer_capacity(_env: *mut JNIEnv, buffer: jobject) -> jlong {
        let buffer = unsafe { &*buffer.cast::<DirectBuffer>() };
        buffer.bytes.len() as jlong
    }

    struct DirectBufferEnv {
        _functions: Box<MaybeUninit<JNINativeInterface__1_6>>,
        env: JNIEnv,
    }

    impl DirectBufferEnv {
        fn new() -> Self {
            let mut functions = Box::new(MaybeUninit::<JNINativeInterface__1_6>::uninit());
            let functions_ptr = functions.as_mut_ptr();
            unsafe {
                std::ptr::addr_of_mut!((*functions_ptr).GetDirectBufferAddress)
                    .write(direct_buffer_address);
                std::ptr::addr_of_mut!((*functions_ptr).GetDirectBufferCapacity)
                    .write(direct_buffer_capacity);
            }
            Self {
                env: functions_ptr.cast::<JNINativeInterface_>(),
                _functions: functions,
            }
        }

        fn as_jni_env(&mut self) -> *mut JNIEnv {
            &mut self.env
        }
    }

    fn vector_hex(vector: &str, key: &str) -> Vec<u8> {
        let marker = format!("\"{key}\": \"");
        let start = vector.find(&marker).expect("D0 vector key") + marker.len();
        let end = vector[start..].find('"').expect("D0 hex terminator") + start;
        let hex = &vector[start..end];
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("D0 hex"))
            .collect()
    }

    fn vector_string<'a>(vector: &'a str, key: &str) -> &'a str {
        let marker = format!("\"{key}\": \"");
        let start = vector.find(&marker).expect("D0 vector key") + marker.len();
        let end = vector[start..].find('"').expect("D0 string terminator") + start;
        &vector[start..end]
    }

    #[test]
    fn misaligned_direct_outcome_buffer_is_rejected_before_typed_write() {
        let mut storage = vec![0_u8; std::mem::size_of::<OutcomeV1>() + 1];
        let misaligned = unsafe { storage.as_mut_ptr().add(1) };
        assert!(
            !misaligned
                .addr()
                .is_multiple_of(std::mem::align_of::<OutcomeV1>()),
            "test fixture must be misaligned"
        );
        assert!(!outcome_buffer_is_valid(
            misaligned,
            std::mem::size_of::<OutcomeV1>() as u64,
        ));
    }

    #[test]
    fn jni_output_ownership_consumes_the_android_create_endpoint() {
        let vector = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/v1/vectors/bootstrap-android-descriptor.json"
        ));
        let descriptor = vector_hex(vector, "launch_descriptor_cbor_hex");
        let expected = vector_string(vector, "localabstract_name");
        let input = CreateInputV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<CreateInputV1>() as u32,
            descriptor_cbor: descriptor.as_ptr(),
            descriptor_len: descriptor.len() as u64,
        };
        let mut handle = 0;
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_create(&input, &mut handle, &mut outcome) },
            STATUS_EVENT
        );
        assert_ne!(outcome.output, 0);
        assert_eq!(
            unsafe {
                native_output_count(std::ptr::null_mut(), std::ptr::null_mut(), handle as i64)
            },
            1
        );
        assert_eq!(
            unsafe {
                native_output_len(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    outcome.output as i64,
                )
            },
            expected.len() as i64
        );
        assert_eq!(outcome.kind, OUTCOME_ENDPOINT_READY);
        assert_eq!(outcome.value0, 1);
        assert_eq!(outcome.value1, 0);
        let mut destination = DirectBuffer {
            bytes: vec![0; expected.len()],
        };
        let mut env = DirectBufferEnv::new();
        assert_eq!(
            unsafe {
                native_output_copy(
                    env.as_jni_env(),
                    std::ptr::null_mut(),
                    outcome.output as jlong,
                    destination.as_jobject(),
                    destination.bytes.len() as jlong,
                )
            },
            expected.len() as jlong
        );
        assert_eq!(destination.bytes, expected.as_bytes());
        assert!(!destination.bytes.contains(&0));
        assert_eq!(
            unsafe {
                native_output_drop(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    outcome.output as i64,
                )
            },
            STATUS_OK
        );
        assert_eq!(
            unsafe {
                native_output_len(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    outcome.output as i64,
                )
            },
            jlong::from(STATUS_INVALID_HANDLE)
        );
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }
}
