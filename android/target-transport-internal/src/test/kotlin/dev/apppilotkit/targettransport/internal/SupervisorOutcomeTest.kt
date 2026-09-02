package dev.apppilotkit.targettransport.internal

import org.junit.Assert.assertEquals
import org.junit.Test

class SupervisorOutcomeTest {
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
