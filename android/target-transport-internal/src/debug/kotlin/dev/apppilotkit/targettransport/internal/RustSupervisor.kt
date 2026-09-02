package dev.apppilotkit.targettransport.internal

import dev.apppilotkit.transport.NativeTransport
import java.nio.ByteBuffer
import java.nio.ByteOrder

internal object TransportAbi {
    // apk_tp_outcome_v1 has eight u32 fields and ten u64 fields, including reserved[4].
    private const val OUTCOME_U32_FIELDS = 8
    private const val OUTCOME_U64_FIELDS = 10
    private const val U32_BYTES = 4
    private const val U64_BYTES = 8

    const val VERSION = 0x0001_0000
    const val STATUS_OK = 0
    const val STATUS_EVENT = 2
    const val STATUS_BUSY = -5
    const val OUTCOME_SIZE = OUTCOME_U32_FIELDS * U32_BYTES + OUTCOME_U64_FIELDS * U64_BYTES

    const val EVENT_BOOTSTRAP_CONNECTED = 1
    const val EVENT_STREAM_BYTES = 2
    const val EVENT_FULL_WRITE_COMMITTED = 3
    const val EVENT_SESSION_ACCEPTED = 4
    const val EVENT_RUNTIME_RESPONSE = 5
    const val EVENT_STREAM_EOF = 6
    const val EVENT_STREAM_IO_FAILED = 7
    const val EVENT_TIMER_FIRED = 9
    const val EVENT_ELIGIBILITY_LOST = 10
    const val EVENT_INTERNAL_ERROR = 12

    const val OUTCOME_ENDPOINT_READY = 1
    const val OUTCOME_WRITE_FRAMES = 2
    const val OUTCOME_APPLICATION = 3
    const val OUTCOME_LEASE_READY = 4
    const val OUTCOME_NEED_INPUT = 5
    const val OUTCOME_SESSION_TERMINAL = 6
    const val OUTCOME_LEASE_TERMINAL = 7
    const val OUTCOME_CLOSED = 8

    const val OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0 = 1 shl 1
    const val OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN = 1 shl 2
}

internal data class SupervisorEvent(
    val tag: Int,
    val streamId: Long = 0,
    val writeToken: Long = 0,
    val bytes: ByteArray = ByteArray(0),
)

internal data class SupervisorOutcome(
    val kind: Int,
    val flags: Int,
    val streamId: Long,
    val writeToken: Long,
    var bytes: ByteArray?,
    val value0: Long,
    val value1: Long,
    val nextDeadlineMilliseconds: Long,
    val closeReason: Int,
    val handoffState: Int,
) {
    val deadlineToken: Long?
        get() = when {
            flags and TransportAbi.OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0 != 0 -> value0
            flags and TransportAbi.OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN != 0 -> writeToken
            else -> null
        }
}

internal class TransportFfiException(val status: Int) : IllegalStateException("Target transport FFI failed: $status")

/** Serial callers own this handle; C1 remains the authority for every transport state transition. */
internal class RustSupervisor(descriptor: ByteArray) : AutoCloseable {
    private var handle = 0L
    val initialOutcome: SupervisorOutcome

    init {
        NativeTransport.ensureLoaded()
        if (NativeTransport.abiVersion() != TransportAbi.VERSION) throw TransportFfiException(-1)
        val outcome = outcomeBuffer()
        val descriptorBuffer = directBuffer(descriptor)
        try {
            val created = NativeTransport.create(descriptorBuffer, descriptor.size.toLong(), outcome)
            if (created <= 0) throw TransportFfiException(created.toInt())
            handle = created
            initialOutcome = copyOutcome(outcome)
        } catch (failure: Throwable) {
            if (handle != 0L) NativeTransport.drop(handle)
            throw failure
        } finally {
            wipe(descriptorBuffer)
        }
    }

    fun drive(event: SupervisorEvent): SupervisorOutcome {
        check(handle != 0L)
        val outcome = outcomeBuffer()
        val byteBuffer = if (event.bytes.isEmpty()) null else directBuffer(event.bytes)
        try {
            var status = NativeTransport.drive(
                handle,
                event.tag,
                0,
                event.streamId,
                event.writeToken,
                byteBuffer,
                event.bytes.size.toLong(),
                outcome,
            )
            repeat(64) {
                if (status != TransportAbi.STATUS_BUSY) return@repeat
                Thread.yield()
                status = NativeTransport.drive(
                    handle, event.tag, 0, event.streamId, event.writeToken,
                    byteBuffer, event.bytes.size.toLong(), outcome,
                )
            }
            if (status < TransportAbi.STATUS_OK) throw TransportFfiException(status)
            return copyOutcome(outcome)
        } finally {
            byteBuffer?.let(::wipe)
        }
    }

    override fun close() {
        if (handle == 0L) return
        val outcome = outcomeBuffer()
        var status = NativeTransport.close(handle, outcome)
        repeat(64) {
            if (status != TransportAbi.STATUS_BUSY) return@repeat
            Thread.yield()
            status = NativeTransport.close(handle, outcome)
        }
        val closing = handle
        handle = 0
        if (status != TransportAbi.STATUS_OK) {
            NativeTransport.drop(closing)
            throw TransportFfiException(status)
        }
        copyOutcome(outcome).bytes?.fill(0)
    }

    private fun copyOutcome(buffer: ByteBuffer): SupervisorOutcome {
        buffer.order(ByteOrder.nativeOrder()).rewind()
        val abiVersion = buffer.int
        val structSize = buffer.int
        if (abiVersion != TransportAbi.VERSION || structSize < TransportAbi.OUTCOME_SIZE) {
            throw TransportFfiException(-2)
        }
        val kind = buffer.int
        val flags = buffer.int
        val streamId = buffer.long
        val writeToken = buffer.long
        var output = buffer.long
        val value0 = buffer.long
        val value1 = buffer.long
        val deadline = buffer.long
        val closeReason = buffer.int
        val handoffState = buffer.int
        buffer.int // peer close reason: C1 owns it; Target never reinterprets it.
        buffer.int // peer handoff state
        repeat(4) { buffer.long }

        val bytes = if (output == 0L) {
            null
        } else {
            try {
                val length = NativeTransport.outputLen(output)
                if (length < 0 || length > Int.MAX_VALUE) throw TransportFfiException(length.toInt())
                val destination = ByteBuffer.allocateDirect(length.toInt())
                try {
                    val copied = NativeTransport.outputCopy(output, destination, length)
                    if (copied != length) throw TransportFfiException(copied.toInt())
                    ByteArray(length.toInt()).also { result ->
                        destination.rewind()
                        destination.get(result)
                    }
                } finally {
                    wipe(destination)
                }
            } finally {
                val status = NativeTransport.outputDrop(output)
                output = 0
                if (status != TransportAbi.STATUS_OK) throw TransportFfiException(status)
            }
        }
        return SupervisorOutcome(kind, flags, streamId, writeToken, bytes, value0, value1, deadline, closeReason, handoffState)
    }

    private fun outcomeBuffer(): ByteBuffer = ByteBuffer.allocateDirect(TransportAbi.OUTCOME_SIZE)
        .order(ByteOrder.nativeOrder())

    private fun directBuffer(bytes: ByteArray): ByteBuffer = ByteBuffer.allocateDirect(bytes.size)
        .order(ByteOrder.nativeOrder())
        .put(bytes)
        .flip() as ByteBuffer

    private fun wipe(buffer: ByteBuffer) {
        buffer.clear()
        while (buffer.hasRemaining()) buffer.put(0)
        buffer.clear()
    }
}
