use apppilotkit_transport_crypto_core::{
    BootstrapBinding, BrokerBootstrap, BrokerBootstrapAckReceiver, BrokerLeaseConnection,
    BrokerSession, BrokerStaticKeypair, OuterFrameDecoder, ProcessBootstrapSecret, SessionBinding,
};
use minicbor::Encoder;
use std::{ptr, slice};
use zeroize::Zeroize;

struct Broker {
    bootstrap: Option<BrokerBootstrap<'static>>,
    bootstrap_ack: Option<BrokerBootstrapAckReceiver>,
    lease: Option<BrokerLeaseConnection>,
    pbs: *mut ProcessBootstrapSecret,
    session_binding: Option<SessionBinding>,
    session: Option<BrokerSession>,
    response_decoder: OuterFrameDecoder,
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.bootstrap = None;
        self.bootstrap_ack = None;
        self.lease = None;
        self.session = None;
        if !self.pbs.is_null() {
            // SAFETY: create transfers this Box exactly once; dependent bootstrap is dropped above.
            unsafe { drop(Box::from_raw(self.pbs)) };
            self.pbs = ptr::null_mut();
        }
    }
}

fn binding() -> BootstrapBinding {
    BootstrapBinding {
        target_reference_digest: [0x41; 32],
        lease_id: [0x51; 16],
        target_nonce: [0x61; 32],
        app_artifact_digest: [0x71; 32],
        expiry_ms: 1_893_456_000_000,
    }
}

fn input<'a>(bytes: *const u8, len: u64) -> Result<&'a [u8], ()> {
    let len = usize::try_from(len).map_err(|_| ())?;
    if len == 0 {
        return Ok(&[]);
    }
    if bytes.is_null() {
        return Err(());
    }
    // SAFETY: the synchronous C caller keeps the buffer alive for this call.
    Ok(unsafe { slice::from_raw_parts(bytes, len) })
}

fn broker(handle: u64) -> Result<&'static mut Broker, ()> {
    if handle == 0 {
        return Err(());
    }
    // SAFETY: the test wrapper serializes calls and owns this Box handle.
    Ok(unsafe { &mut *(handle as *mut Broker) })
}

fn publish(bytes: Vec<u8>, destination: *mut u64) -> Result<(), ()> {
    if destination.is_null() {
        return Err(());
    }
    // SAFETY: destination is non-null caller storage.
    unsafe { *destination = Box::into_raw(Box::new(bytes)) as u64 };
    Ok(())
}

fn descriptor(binding: &BootstrapBinding, public_key: [u8; 32], port: u16) -> Result<Vec<u8>, ()> {
    if port < 49_152 {
        return Err(());
    }
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes);
    e.map(9)
        .map_err(|_| ())?
        .u8(0)
        .map_err(|_| ())?
        .u8(1)
        .map_err(|_| ())?
        .u8(1)
        .map_err(|_| ())?
        .u8(0)
        .map_err(|_| ())?
        .u8(2)
        .map_err(|_| ())?
        .bytes(&binding.lease_id)
        .map_err(|_| ())?
        .u8(3)
        .map_err(|_| ())?
        .bytes(&binding.target_nonce)
        .map_err(|_| ())?
        .u8(4)
        .map_err(|_| ())?
        .bytes(&binding.app_artifact_digest)
        .map_err(|_| ())?
        .u8(5)
        .map_err(|_| ())?
        .bytes(&public_key)
        .map_err(|_| ())?
        .u8(6)
        .map_err(|_| ())?
        .map(2)
        .map_err(|_| ())?
        .u8(0)
        .map_err(|_| ())?
        .str("127.0.0.1")
        .map_err(|_| ())?
        .u8(1)
        .map_err(|_| ())?
        .u16(port)
        .map_err(|_| ())?
        .u8(7)
        .map_err(|_| ())?
        .u64(binding.expiry_ms)
        .map_err(|_| ())?
        .u8(8)
        .map_err(|_| ())?
        .bytes(&binding.target_reference_digest)
        .map_err(|_| ())?;
    Ok(bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_create(
    port: u16,
    out_handle: *mut u64,
    out_descriptor: *mut u64,
) -> i32 {
    let result = (|| {
        if out_handle.is_null() || out_descriptor.is_null() {
            return Err(());
        }
        let binding = binding();
        let keypair = BrokerStaticKeypair::generate().map_err(|_| ())?;
        let encoded = descriptor(&binding, keypair.public_key(), port)?;
        let pbs = Box::into_raw(Box::new(
            ProcessBootstrapSecret::generate().map_err(|_| ())?,
        ));
        // SAFETY: pbs remains allocated until Broker::drop and bootstrap is dropped first.
        let pbs_ref: &'static ProcessBootstrapSecret = unsafe { &*pbs };
        let bootstrap =
            BrokerBootstrap::new(binding, keypair.into_private_key(), pbs_ref).map_err(|_| ())?;
        let state = Box::new(Broker {
            bootstrap: Some(bootstrap),
            bootstrap_ack: None,
            lease: None,
            pbs,
            session_binding: None,
            session: None,
            response_decoder: OuterFrameDecoder::new(),
        });
        // SAFETY: output pointers were checked.
        unsafe { *out_handle = Box::into_raw(state) as u64 };
        publish(encoded, out_descriptor)
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_bootstrap_m1(
    handle: u64,
    bytes: *const u8,
    len: u64,
    out_m2: *mut u64,
) -> i32 {
    let result = (|| {
        let state = broker(handle)?;
        let bootstrap = state.bootstrap.take().ok_or(())?;
        let (m2, ack) = bootstrap
            .read_m1_write_m2(input(bytes, len)?)
            .map_err(|_| ())?;
        state.bootstrap_ack = Some(ack);
        publish(m2, out_m2)
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_bootstrap_ack(handle: u64, bytes: *const u8, len: u64) -> i32 {
    let result = (|| {
        let state = broker(handle)?;
        let receiver = state.bootstrap_ack.take().ok_or(())?;
        let (ack, lease) = receiver.read_ack(input(bytes, len)?).map_err(|_| ())?;
        state.session_binding = Some(SessionBinding {
            lease_id: ack.lease_id,
            process_generation: ack.process_generation,
            listener_epoch: ack.listener_epoch,
            nk_handshake_hash: ack.nk_handshake_hash,
        });
        state.lease = Some(lease);
        Ok::<(), ()>(())
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_heartbeat(
    handle: u64,
    counter: u64,
    out_frame: *mut u64,
) -> i32 {
    let result = (|| {
        let lease = broker(handle)?.lease.as_mut().ok_or(())?;
        publish(
            lease.write_heartbeat_request(counter).map_err(|_| ())?,
            out_frame,
        )
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_heartbeat_reply(
    handle: u64,
    bytes: *const u8,
    len: u64,
    expected_counter: u64,
) -> i32 {
    let result = (|| {
        let lease = broker(handle)?.lease.as_mut().ok_or(())?;
        let counter = lease
            .read_heartbeat_reply(input(bytes, len)?)
            .map_err(|_| ())?;
        if counter != expected_counter {
            return Err(());
        }
        Ok::<(), ()>(())
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_session_m1(
    handle: u64,
    bytes: *const u8,
    len: u64,
    out_m2: *mut u64,
) -> i32 {
    let result = (|| {
        let state = broker(handle)?;
        let pbs = unsafe { &*state.pbs };
        let mut session =
            BrokerSession::new(state.session_binding.clone().ok_or(())?, pbs).map_err(|_| ())?;
        let m2 = session
            .read_m1_write_m2(input(bytes, len)?)
            .map_err(|_| ())?;
        state.session = Some(session);
        publish(m2, out_m2)
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_target_finished(
    handle: u64,
    bytes: *const u8,
    len: u64,
    out_finished: *mut u64,
) -> i32 {
    let result = (|| {
        let session = broker(handle)?.session.as_mut().ok_or(())?;
        session.read_finished(input(bytes, len)?).map_err(|_| ())?;
        publish(session.write_finished().map_err(|_| ())?, out_finished)
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_session_open(
    handle: u64,
    bytes: *const u8,
    len: u64,
    out_frames: *mut u64,
) -> i32 {
    let result = (|| {
        let session = broker(handle)?.session.as_mut().ok_or(())?;
        let frames = session
            .write_session_open(input(bytes, len)?)
            .map_err(|_| ())?;
        publish(frames.concat(), out_frames)
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_session_response(
    handle: u64,
    bytes: *const u8,
    len: u64,
    out_plaintext: *mut u64,
) -> i32 {
    let result = (|| {
        let state = broker(handle)?;
        let frames = state
            .response_decoder
            .push(input(bytes, len)?)
            .map_err(|_| ())?;
        let session = state.session.as_mut().ok_or(())?;
        let mut complete = None;
        for frame in frames {
            if let Some(value) = session.read_application_response(&frame).map_err(|_| ())? {
                complete = Some(value);
            }
        }
        publish(complete.ok_or(())?, out_plaintext)
    })();
    if result.is_ok() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_output_len(output: u64, out_len: *mut u64) -> i32 {
    if output == 0 || out_len.is_null() {
        return -1;
    }
    let bytes = unsafe { &*(output as *const Vec<u8>) };
    unsafe { *out_len = bytes.len() as u64 };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_output_copy(
    output: u64,
    destination: *mut u8,
    capacity: u64,
) -> i32 {
    if output == 0 || destination.is_null() {
        return -1;
    }
    let bytes = unsafe { &*(output as *const Vec<u8>) };
    if capacity < bytes.len() as u64 {
        return -1;
    }
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len()) };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_output_drop(output: *mut u64) -> i32 {
    if output.is_null() {
        return -1;
    }
    let value = unsafe { *output };
    if value == 0 {
        return -1;
    }
    let mut bytes = unsafe { Box::from_raw(value as *mut Vec<u8>) };
    bytes.zeroize();
    unsafe { *output = 0 };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn apk_tp_test_broker_drop(handle: *mut u64) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let value = unsafe { *handle };
    if value == 0 {
        return -1;
    }
    unsafe {
        drop(Box::from_raw(value as *mut Broker));
        *handle = 0;
    }
    0
}
