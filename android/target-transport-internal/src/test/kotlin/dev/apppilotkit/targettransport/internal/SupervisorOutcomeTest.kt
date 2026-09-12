package dev.apppilotkit.targettransport.internal

import org.junit.Assert.assertEquals
import org.junit.Test

class SupervisorOutcomeTest {
    @Test
    fun `Android endpoint readiness accepts emulator and Android device platforms`() {
        assertEquals(true, isAndroidEndpointReady(TransportAbi.OUTCOME_ENDPOINT_READY, 1, 0))
        assertEquals(true, isAndroidEndpointReady(TransportAbi.OUTCOME_ENDPOINT_READY, 3, 0))
    }

    @Test
    fun `Android endpoint readiness rejects other platform and outcome combinations`() {
        assertEquals(false, isAndroidEndpointReady(TransportAbi.OUTCOME_ENDPOINT_READY, 0, 12_345))
        assertEquals(false, isAndroidEndpointReady(TransportAbi.OUTCOME_ENDPOINT_READY, 2, 12_345))
        assertEquals(false, isAndroidEndpointReady(TransportAbi.OUTCOME_NEED_INPUT, 3, 0))
    }

    @Test
    fun `C ABI value0 deadline flag selects value0 instead of write token`() {
        assertEquals(1 shl 1, TransportAbi.OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0)

        val outcome = SupervisorOutcome(
            kind = TransportAbi.OUTCOME_NEED_INPUT,
            flags = 1 shl 1,
            streamId = 7,
            writeToken = 101,
            bytes = null,
            value0 = 202,
            value1 = 0,
            nextDeadlineMilliseconds = 1_000,
            closeReason = 0,
            handoffState = 0,
        )

        assertEquals(202L, outcome.deadlineToken)
    }
}
