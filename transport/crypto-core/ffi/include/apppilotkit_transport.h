#ifndef APPPILOTKIT_TRANSPORT_H
#define APPPILOTKIT_TRANSPORT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define APPPILOTKIT_TP_ABI_VERSION_V1 UINT32_C(0x00010000)

typedef uint64_t apk_tp_handle_v1;
typedef uint64_t apk_tp_output_v1;
typedef int32_t apk_tp_status_v1;

enum {
  APK_TP_STATUS_OK = 0,
  APK_TP_STATUS_NEED_INPUT = 1,
  APK_TP_STATUS_EVENT = 2,
  APK_TP_STATUS_TERMINAL = 3,
  APK_TP_STATUS_ABI_MISMATCH = -1,
  APK_TP_STATUS_INVALID_ARGUMENT = -2,
  APK_TP_STATUS_INVALID_HANDLE = -3,
  APK_TP_STATUS_WRONG_PHASE = -4,
  /* Retry the same event. Terminal/timer events may already be latched; retries are idempotent. */
  APK_TP_STATUS_BUSY = -5,
  APK_TP_STATUS_BUFFER_TOO_SMALL = -6,
  APK_TP_STATUS_INTERNAL_PANIC = -7
};

enum {
  APK_TP_EVENT_BOOTSTRAP_CONNECTED = 1,
  APK_TP_EVENT_STREAM_BYTES = 2,
  APK_TP_EVENT_FULL_WRITE_COMMITTED = 3,
  APK_TP_EVENT_SESSION_ACCEPTED = 4,
  APK_TP_EVENT_RUNTIME_RESPONSE = 5,
  APK_TP_EVENT_STREAM_EOF = 6,
  APK_TP_EVENT_STREAM_IO_FAILED = 7,
  APK_TP_EVENT_STREAM_CLOSE_NORMAL = 8,
  APK_TP_EVENT_TIMER_FIRED = 9,
  APK_TP_EVENT_ELIGIBILITY_LOST = 10,
  APK_TP_EVENT_CLEANUP_FAILED = 11,
  /* Target adapter invariant or other non-peer implementation failure. */
  APK_TP_EVENT_INTERNAL_ERROR = 12
};

enum {
  APK_TP_OUTCOME_ENDPOINT_READY = 1,
  APK_TP_OUTCOME_WRITE_FRAMES = 2,
  APK_TP_OUTCOME_APPLICATION = 3,
  APK_TP_OUTCOME_LEASE_READY = 4,
  APK_TP_OUTCOME_NEED_INPUT = 5,
  APK_TP_OUTCOME_SESSION_TERMINAL = 6,
  APK_TP_OUTCOME_LEASE_TERMINAL = 7,
  APK_TP_OUTCOME_CLOSED = 8
};

enum {
  APK_TP_OUTCOME_FLAG_PEER_CLOSE = 1u << 0,
  APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0 = 1u << 1,
  APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN = 1u << 2,
  APK_TP_OUTCOME_FLAG_TERMINAL_AFTER_WRITE_COMMIT = 1u << 3
};

enum {
  APK_TP_CLOSE_NORMAL = 0,
  APK_TP_CLOSE_AUTHENTICATION_FAILED = 1,
  APK_TP_CLOSE_BINDING_MISMATCH = 2,
  APK_TP_CLOSE_STALE = 3,
  APK_TP_CLOSE_TIMEOUT = 4,
  APK_TP_CLOSE_OVERSIZE = 5,
  APK_TP_CLOSE_MALFORMED = 6,
  APK_TP_CLOSE_SEQUENCE_VIOLATION = 7,
  APK_TP_CLOSE_RECORD_LIMIT = 8,
  APK_TP_CLOSE_PEER_CLOSED = 9,
  APK_TP_CLOSE_BROKER_LOST = 10,
  APK_TP_CLOSE_ELIGIBILITY_LOST = 11,
  APK_TP_CLOSE_CLEANUP_FAILED = 12,
  APK_TP_CLOSE_INTERNAL_ERROR = 13
};

enum {
  APK_TP_HANDOFF_NOT_HANDED_OFF = 0,
  APK_TP_HANDOFF_POSSIBLE_OR_CONFIRMED = 1
};

typedef struct apk_tp_create_input_v1 {
  uint32_t abi_version;
  uint32_t struct_size;
  const uint8_t *descriptor_cbor;
  uint64_t descriptor_len;
} apk_tp_create_input_v1;

typedef struct apk_tp_event_v1 {
  uint32_t abi_version;
  uint32_t struct_size;
  uint32_t tag;
  uint32_t flags;
  uint64_t stream_id;
  uint64_t write_token;
  const uint8_t *bytes;
  uint64_t bytes_len;
} apk_tp_event_v1;

/* ENDPOINT_READY shape:
 * iOS: value0=0, value1=validated loopback port, output=0.
 * Android: value0=1, value1=0, output owns the exact validated 32..96 byte
 * raw UTF-8 localabstract name, without a NUL suffix.
 */
typedef struct apk_tp_outcome_v1 {
  uint32_t abi_version;
  uint32_t struct_size;
  uint32_t kind;
  uint32_t flags;
  uint64_t stream_id;
  uint64_t write_token;
  apk_tp_output_v1 output;
  uint64_t value0;
  uint64_t value1;
  /* Relative monotonic duration from observing this outcome. Its token is one-shot; late tokens are no-ops. */
  uint64_t next_deadline_ms;
  uint32_t close_reason;
  uint32_t handoff_state;
  uint32_t peer_close_reason;
  uint32_t peer_handoff_state;
  uint64_t reserved[4];
} apk_tp_outcome_v1;

uint32_t apppilotkit_tp_v1_abi_version(void);
apk_tp_status_v1 apppilotkit_tp_v1_create(const apk_tp_create_input_v1 *input,
                                           apk_tp_handle_v1 *out_handle,
                                           apk_tp_outcome_v1 *out_outcome);
apk_tp_status_v1 apppilotkit_tp_v1_drive(apk_tp_handle_v1 handle,
                                          const apk_tp_event_v1 *event,
                                          apk_tp_outcome_v1 *out_outcome);
apk_tp_status_v1 apppilotkit_tp_v1_close(apk_tp_handle_v1 *handle,
                                          apk_tp_outcome_v1 *out_outcome);
apk_tp_status_v1 apppilotkit_tp_v1_drop(apk_tp_handle_v1 *handle);
apk_tp_status_v1 apppilotkit_tp_v1_output_count(apk_tp_handle_v1 handle,
                                                 uint64_t *out_count);
apk_tp_status_v1 apppilotkit_tp_v1_output_len(apk_tp_output_v1 output,
                                               uint64_t *out_len);
apk_tp_status_v1 apppilotkit_tp_v1_output_copy(apk_tp_output_v1 output,
                                                uint8_t *destination,
                                                uint64_t capacity,
                                                uint64_t *out_written);
apk_tp_status_v1 apppilotkit_tp_v1_output_drop(apk_tp_output_v1 *output);

#ifdef __cplusplus
}
#endif

#endif
