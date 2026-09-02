#ifndef APPPILOTKIT_TRANSPORT_TEST_BROKER_H
#define APPPILOTKIT_TRANSPORT_TEST_BROKER_H

#include <stdint.h>

typedef uint64_t apk_tp_test_broker_handle;
typedef uint64_t apk_tp_test_broker_output;

int32_t apk_tp_test_broker_create(uint16_t port, apk_tp_test_broker_handle *handle,
                                  apk_tp_test_broker_output *descriptor);
int32_t apk_tp_test_broker_bootstrap_m1(apk_tp_test_broker_handle handle,
                                        const uint8_t *bytes, uint64_t bytes_len,
                                        apk_tp_test_broker_output *m2);
int32_t apk_tp_test_broker_bootstrap_ack(apk_tp_test_broker_handle handle,
                                         const uint8_t *bytes, uint64_t bytes_len);
int32_t apk_tp_test_broker_heartbeat(apk_tp_test_broker_handle handle, uint64_t counter,
                                    apk_tp_test_broker_output *frame);
int32_t apk_tp_test_broker_heartbeat_reply(apk_tp_test_broker_handle handle,
                                           const uint8_t *bytes, uint64_t bytes_len,
                                           uint64_t expected_counter);
int32_t apk_tp_test_broker_session_m1(apk_tp_test_broker_handle handle,
                                      const uint8_t *bytes, uint64_t bytes_len,
                                      apk_tp_test_broker_output *m2);
int32_t apk_tp_test_broker_target_finished(apk_tp_test_broker_handle handle,
                                           const uint8_t *bytes, uint64_t bytes_len,
                                           apk_tp_test_broker_output *finished);
int32_t apk_tp_test_broker_session_open(apk_tp_test_broker_handle handle,
                                        const uint8_t *bytes, uint64_t bytes_len,
                                        apk_tp_test_broker_output *frames);
int32_t apk_tp_test_broker_session_response(apk_tp_test_broker_handle handle,
                                            const uint8_t *bytes, uint64_t bytes_len,
                                            apk_tp_test_broker_output *plaintext);
int32_t apk_tp_test_broker_output_len(apk_tp_test_broker_output output, uint64_t *len);
int32_t apk_tp_test_broker_output_copy(apk_tp_test_broker_output output,
                                       uint8_t *destination, uint64_t capacity);
int32_t apk_tp_test_broker_output_drop(apk_tp_test_broker_output *output);
int32_t apk_tp_test_broker_drop(apk_tp_test_broker_handle *handle);

#endif
