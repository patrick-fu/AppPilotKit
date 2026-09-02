#include "apppilotkit_transport.h"

#include <assert.h>
#include <stdlib.h>
#include <string.h>

_Static_assert(sizeof(apk_tp_create_input_v1) == 24, "create ABI layout");
_Static_assert(sizeof(apk_tp_event_v1) == 48, "event ABI layout");
_Static_assert(sizeof(apk_tp_outcome_v1) == 112, "outcome ABI layout");
_Static_assert(APK_TP_CLOSE_INTERNAL_ERROR == 13, "close reason ABI");
_Static_assert(APK_TP_HANDOFF_POSSIBLE_OR_CONFIRMED == 1, "handoff ABI");

static uint8_t nibble(char value) {
  if (value >= '0' && value <= '9') return (uint8_t)(value - '0');
  if (value >= 'a' && value <= 'f') return (uint8_t)(value - 'a' + 10);
  abort();
}

static size_t decode_hex(const char *hex, uint8_t *out) {
  size_t length = strlen(hex) / 2;
  for (size_t index = 0; index < length; ++index) {
    out[index] = (uint8_t)((nibble(hex[index * 2]) << 4) |
                           nibble(hex[index * 2 + 1]));
  }
  return length;
}

int main(void) {
  static const char descriptor_hex[] =
      "a9000101000250515151515151515151515151515151510358207171717171717171"
      "7171717171717171717171717171717171717171717171710458208181818181818181"
      "8181818181818181818181818181818181818181818181810558207b4e909bbe7ffe44"
      "c465a220037d608ee35897d31ef972f07f74892cb0f73f1306a200693132372e302e30"
      "2e310119d6d9071b000001b8dac5b400085820791b63ed11406e77475fafbf092c8dc7"
      "86d728ed0773d7662373741dea079404";
  static const char android_descriptor_hex[] =
      "a9000101010250515151515151515151515151515151510358207171717171717171"
      "7171717171717171717171717171717171717171717171710458208181818181818181"
      "8181818181818181818181818181818181818181818181810558207b4e909bbe7ffe44"
      "c465a220037d608ee35897d31ef972f07f74892cb0f73f1306a100782e61707070696c"
      "6f746b69742d616e64726f69642d626f6f7473747261702d3031323334353637383961"
      "6263646566071b000001b8dac5b400085820791b63ed11406e77475fafbf092c8dc786"
      "d728ed0773d7662373741dea079404";
  static const char android_endpoint[] =
      "apppilotkit-android-bootstrap-0123456789abcdef";
  uint8_t descriptor[256];
  size_t descriptor_len = decode_hex(descriptor_hex, descriptor);

  assert(apppilotkit_tp_v1_abi_version() == APPPILOTKIT_TP_ABI_VERSION_V1);
  apk_tp_create_input_v1 input = {
      .abi_version = APPPILOTKIT_TP_ABI_VERSION_V1,
      .struct_size = sizeof(input),
      .descriptor_cbor = descriptor,
      .descriptor_len = descriptor_len,
  };
  apk_tp_handle_v1 handle = 0;
  apk_tp_outcome_v1 outcome;
  assert(apppilotkit_tp_v1_create(&input, &handle, &outcome) ==
         APK_TP_STATUS_EVENT);
  assert(handle != 0);
  assert(outcome.kind == APK_TP_OUTCOME_ENDPOINT_READY);
  assert(outcome.output == 0 && outcome.value0 == 0 && outcome.value1 == 55001);
  for (size_t index = 0; index < 4; ++index) assert(outcome.reserved[index] == 0);

  apk_tp_event_v1 connected = {
      .abi_version = APPPILOTKIT_TP_ABI_VERSION_V1,
      .struct_size = sizeof(connected),
      .tag = APK_TP_EVENT_BOOTSTRAP_CONNECTED,
      .stream_id = 7,
  };
  assert(apppilotkit_tp_v1_drive(handle, &connected, &outcome) ==
         APK_TP_STATUS_EVENT);
  assert(outcome.kind == APK_TP_OUTCOME_WRITE_FRAMES);
  assert(outcome.output != 0 && outcome.write_token != 0);

  uint64_t count = 0;
  assert(apppilotkit_tp_v1_output_count(handle, &count) == APK_TP_STATUS_OK);
  assert(count == 1);
  uint64_t length = 0;
  assert(apppilotkit_tp_v1_output_len(outcome.output, &length) ==
         APK_TP_STATUS_OK);
  uint8_t tiny = 0;
  uint64_t written = 0;
  assert(apppilotkit_tp_v1_output_copy(outcome.output, &tiny, 1, &written) ==
         APK_TP_STATUS_BUFFER_TOO_SMALL);
  assert(written == length);
  uint8_t *frame = malloc((size_t)length);
  assert(frame != NULL);
  assert(apppilotkit_tp_v1_output_copy(outcome.output, frame, length, &written) ==
         APK_TP_STATUS_OK);
  free(frame);
  apk_tp_output_v1 output = outcome.output;
  assert(apppilotkit_tp_v1_output_drop(&output) == APK_TP_STATUS_OK);
  assert(output == 0);
  assert(apppilotkit_tp_v1_output_drop(&output) == APK_TP_STATUS_OK);

  apk_tp_event_v1 wrong_commit = connected;
  wrong_commit.tag = APK_TP_EVENT_FULL_WRITE_COMMITTED;
  wrong_commit.write_token = outcome.write_token + 1;
  assert(apppilotkit_tp_v1_drive(handle, &wrong_commit, &outcome) ==
         APK_TP_STATUS_WRONG_PHASE);
  wrong_commit.write_token -= 1;
  assert(apppilotkit_tp_v1_drive(handle, &wrong_commit, &outcome) ==
         APK_TP_STATUS_NEED_INPUT);

  apk_tp_handle_v1 stale = handle;
  assert(apppilotkit_tp_v1_close(&handle, &outcome) == APK_TP_STATUS_OK);
  assert(handle == 0 && outcome.kind == APK_TP_OUTCOME_CLOSED);
  assert(apppilotkit_tp_v1_close(&handle, &outcome) == APK_TP_STATUS_OK);
  assert(apppilotkit_tp_v1_drive(stale, &connected, &outcome) ==
         APK_TP_STATUS_INVALID_HANDLE);

  uint8_t android_descriptor[256];
  size_t android_descriptor_len =
      decode_hex(android_descriptor_hex, android_descriptor);
  apk_tp_create_input_v1 android_input = {
      .abi_version = APPPILOTKIT_TP_ABI_VERSION_V1,
      .struct_size = sizeof(android_input),
      .descriptor_cbor = android_descriptor,
      .descriptor_len = android_descriptor_len,
  };
  assert(apppilotkit_tp_v1_create(&android_input, &handle, &outcome) ==
         APK_TP_STATUS_EVENT);
  assert(handle != 0 && outcome.kind == APK_TP_OUTCOME_ENDPOINT_READY);
  assert(outcome.value0 == 1 && outcome.value1 == 0 && outcome.output != 0);
  assert(apppilotkit_tp_v1_output_count(handle, &count) == APK_TP_STATUS_OK);
  assert(count == 1);
  assert(apppilotkit_tp_v1_output_len(outcome.output, &length) ==
         APK_TP_STATUS_OK);
  assert(length == strlen(android_endpoint));
  uint8_t endpoint[97];
  memset(endpoint, 0xa5, sizeof(endpoint));
  assert(apppilotkit_tp_v1_output_copy(outcome.output, endpoint,
                                       sizeof(endpoint), &written) ==
         APK_TP_STATUS_OK);
  assert(written == length);
  assert(memcmp(endpoint, android_endpoint, (size_t)written) == 0);
  assert(endpoint[written] == 0xa5);
  output = outcome.output;
  assert(apppilotkit_tp_v1_output_drop(&output) == APK_TP_STATUS_OK);
  assert(apppilotkit_tp_v1_output_count(handle, &count) == APK_TP_STATUS_OK);
  assert(count == 0);
  assert(apppilotkit_tp_v1_close(&handle, &outcome) == APK_TP_STATUS_OK);
  assert(handle == 0);

  android_descriptor[android_descriptor_len] = 0;
  android_input.descriptor_len = android_descriptor_len + 1;
  assert(apppilotkit_tp_v1_create(&android_input, &handle, &outcome) ==
         APK_TP_STATUS_INVALID_ARGUMENT);
  assert(handle == 0 && outcome.kind == 0 && outcome.output == 0);

  apk_tp_handle_v1 random_handle = UINT64_C(0xffffffffffffffff);
  assert(apppilotkit_tp_v1_drop(&random_handle) == APK_TP_STATUS_INVALID_HANDLE);
  assert(random_handle == 0);
  assert(apppilotkit_tp_v1_create(NULL, &handle, &outcome) ==
         APK_TP_STATUS_INVALID_ARGUMENT);

  assert(apppilotkit_tp_v1_create(&input, &handle, &outcome) ==
         APK_TP_STATUS_EVENT);
  apk_tp_event_v1 internal_error = {
      .abi_version = APPPILOTKIT_TP_ABI_VERSION_V1,
      .struct_size = sizeof(internal_error),
      .tag = APK_TP_EVENT_INTERNAL_ERROR,
      .stream_id = 1,
  };
  assert(apppilotkit_tp_v1_drive(handle, &internal_error, &outcome) ==
         APK_TP_STATUS_INVALID_ARGUMENT);
  apk_tp_event_v1 second_connected = connected;
  second_connected.stream_id = 17;
  assert(apppilotkit_tp_v1_drive(handle, &second_connected, &outcome) ==
         APK_TP_STATUS_EVENT);
  assert(outcome.kind == APK_TP_OUTCOME_WRITE_FRAMES && outcome.output != 0);
  output = outcome.output;
  assert(apppilotkit_tp_v1_output_drop(&output) == APK_TP_STATUS_OK);
  internal_error.stream_id = 0;
  assert(apppilotkit_tp_v1_drive(handle, &internal_error, &outcome) ==
         APK_TP_STATUS_TERMINAL);
  assert(outcome.kind == APK_TP_OUTCOME_LEASE_TERMINAL);
  assert(outcome.close_reason == APK_TP_CLOSE_INTERNAL_ERROR);
  assert(outcome.output == 0 && outcome.peer_close_reason == 0 &&
         outcome.peer_handoff_state == 0);
  assert(apppilotkit_tp_v1_drop(&handle) == APK_TP_STATUS_OK);
  assert(handle == 0);
  return 0;
}
