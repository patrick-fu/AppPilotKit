package dev.apppilotkit.targettransport.internal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PendingSessionAdmissionTest {
    @Test
    fun `first session defers until the lease is ready and drains once`() {
        val admission = PendingSessionAdmission()

        assertTrue(admission.defer(17))
        assertEquals(17L, admission.takeWhenReady())
        assertNull(admission.takeWhenReady())
    }

    @Test
    fun `second session is rejected while the first is deferred`() {
        val admission = PendingSessionAdmission()

        assertTrue(admission.defer(17))
        assertFalse(admission.defer(23))
        assertEquals(17L, admission.takeWhenReady())
    }

    @Test
    fun `terminal transition clears a deferred session`() {
        val admission = PendingSessionAdmission()

        assertTrue(admission.defer(17))
        admission.clear()

        assertNull(admission.takeWhenReady())
    }
}
