package dev.apppilotkit.targettransport.internal

import android.app.Activity
import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.os.SystemClock
import android.util.Log
import dev.apppilotkit.semantic.SemanticRegistry
import dev.apppilotkit.semantic.TargetActionCoordinator
import dev.apppilotkit.semantic.runtime.ProtocolRuntime
import dev.apppilotkit.semantic.runtime.ProtocolRuntimeLimits
import dev.apppilotkit.semantic.runtime.SemanticProtocolPolicy
import java.io.Closeable
import java.io.InputStream
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.util.Base64
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import kotlin.math.max

/**
 * Debug-only runtime inputs. The catalog generation is C1's process generation,
 * so it cannot accidentally introduce a second generation domain.
 */
data class TargetRuntimeComposition(
    val catalog: SemanticRegistry,
    val limits: ProtocolRuntimeLimits,
    val policy: SemanticProtocolPolicy,
    val actionCoordinator: TargetActionCoordinator,
    val targetId: String,
) {
    fun makeRuntime(processGeneration: Long): ProtocolRuntime {
        require(catalog.identity.generation == processGeneration)
        return ProtocolRuntime(catalog, limits, policy, actionCoordinator, targetId)
    }
}

typealias TargetRuntimeCompositionFactory = (processGeneration: Long) -> TargetRuntimeComposition

class TargetTransportException(message: String, cause: Throwable? = null) : IllegalStateException(message, cause)

/**
 * A Debug/Internal-only Android Target composition. C1 owns descriptor validation,
 * Noise, framing, bindings, state transitions and deadlines; Kotlin only applies
 * C1 outcomes on one lifecycle thread and performs Unix-domain socket I/O.
 */
class TargetTransport private constructor(
    private val supervisor: RustSupervisor,
    private val endpointName: String,
    private val compositionFactory: TargetRuntimeCompositionFactory,
) : Closeable {
    private val lifecycleThread = HandlerThread("apppilotkit-target-transport").apply { start() }
    private val lifecycle = Handler(lifecycleThread.looper)
    private val timers: ScheduledExecutorService = Executors.newSingleThreadScheduledExecutor { runnable ->
        Thread(runnable, "apppilotkit-target-transport-timers").apply { isDaemon = true }
    }
    private val socketHost = AbstractSocketHost(::socketEvent)
    private val pendingTimers = linkedMapOf<Long, ScheduledFuture<*>>()
    private val runtimes = linkedMapOf<Long, ProtocolRuntime>()
    private val pendingWrites = linkedMapOf<Long, PendingWrite>()
    private val terminalObserved = ConcurrentHashMap.newKeySet<Long>()

    private var started = false
    private var stopped = false
    private var bootstrapStreamId = 0L
    private var composition: TargetRuntimeComposition? = null
    private var internalFailureDiagnosticLogged = false
    private var leaseTerminalDiagnosticLogged = false
    private val pendingSessionAdmission = PendingSessionAdmission()

    companion object {
        const val DESCRIPTOR_EXTRA = "dev.apppilotkit.transport.DESCRIPTOR"
        private const val TERMINAL_DIAGNOSTIC_TAG = "AppPilotKitTerminal"

        private enum class InternalFailureOrigin(val wireName: String) {
            LISTENER_START_EXCEPTION("listener_start_exception"),
            LISTENER_FAILED("listener_failed"),
            PEER_UNSUPPORTED("peer_unsupported"),
            ACCEPTED_STREAM_INVALID("accepted_stream_invalid"),
            SUPERVISOR_DRIVE_EXCEPTION("supervisor_drive_exception"),
            OUTCOME_PROCESSING_EXCEPTION("outcome_processing_exception"),
            OUTCOME_UNKNOWN("outcome_unknown"),
            WRITE_OUTCOME_INVALID("write_outcome_invalid"),
            APPLICATION_OUTCOME_INVALID("application_outcome_invalid"),
            APPLICATION_RUNTIME_EXCEPTION("application_runtime_exception"),
            LEASE_READY_OUTCOME_INVALID("lease_ready_outcome_invalid"),
            LEASE_READY_COMPOSITION_EXCEPTION("lease_ready_composition_exception"),
            INTERNAL_ERROR_DRIVE_EXCEPTION("internal_error_drive_exception"),
        }

        private fun terminalEventName(tag: Int): String = when (tag) {
            TransportAbi.EVENT_BOOTSTRAP_CONNECTED -> "bootstrap_connected"
            TransportAbi.EVENT_STREAM_BYTES -> "stream_bytes"
            TransportAbi.EVENT_FULL_WRITE_COMMITTED -> "full_write_committed"
            TransportAbi.EVENT_SESSION_ACCEPTED -> "session_accepted"
            TransportAbi.EVENT_RUNTIME_RESPONSE -> "runtime_response"
            TransportAbi.EVENT_STREAM_EOF -> "stream_eof"
            TransportAbi.EVENT_STREAM_IO_FAILED -> "stream_io_failed"
            TransportAbi.EVENT_TIMER_FIRED -> "timer_fired"
            TransportAbi.EVENT_ELIGIBILITY_LOST -> "eligibility_lost"
            TransportAbi.EVENT_INTERNAL_ERROR -> "internal_error"
            else -> "unknown"
        }

        private fun closeReasonName(reason: Int): String = when (reason) {
            0 -> "normal"
            1 -> "authentication_failed"
            2 -> "binding_mismatch"
            3 -> "stale"
            4 -> "timeout"
            5 -> "oversize"
            6 -> "malformed"
            7 -> "sequence_violation"
            8 -> "record_limit"
            9 -> "peer_closed"
            10 -> "broker_lost"
            11 -> "eligibility_lost"
            12 -> "cleanup_failed"
            13 -> "internal_error"
            else -> "unknown"
        }

        /** The caller owns the non-secret descriptor Activity extra and the app composition factory. */
        @JvmStatic
        fun start(descriptor: String, compositionFactory: TargetRuntimeCompositionFactory): TargetTransport {
            val decoded = decodeCanonicalDescriptor(descriptor)
            try {
                val supervisor = RustSupervisor(decoded)
                val endpointBytes = supervisor.initialOutcome.bytes
                    ?: run {
                        supervisor.close()
                        throw TargetTransportException("C1 did not return an Android localabstract endpoint")
                    }
                val endpoint = try {
                    strictUtf8(endpointBytes)
                } finally {
                    endpointBytes.fill(0)
                    supervisor.initialOutcome.bytes = null
                }
                val transport = TargetTransport(supervisor, endpoint, compositionFactory)
                try {
                    transport.runOnLifecycleAndWait { transport.activate() }
                } catch (failure: Throwable) {
                    transport.close()
                    throw failure
                }
                return transport
            } finally {
                decoded.fill(0)
            }
        }

        private fun decodeCanonicalDescriptor(encoded: String): ByteArray {
            if (encoded.isEmpty() || encoded.length % 4 == 1 || encoded.contains('=') ||
                encoded.any { !it.isAsciiLetterOrDigit() && it != '-' && it != '_' }
            ) throw TargetTransportException("Invalid target transport descriptor")
            val decoded = try {
                Base64.getUrlDecoder().decode(encoded)
            } catch (failure: IllegalArgumentException) {
                throw TargetTransportException("Invalid target transport descriptor", failure)
            }
            val canonical = Base64.getUrlEncoder().withoutPadding().encodeToString(decoded)
            if (canonical != encoded) {
                decoded.fill(0)
                throw TargetTransportException("Invalid target transport descriptor")
            }
            return decoded
        }

        private fun strictUtf8(bytes: ByteArray): String = try {
            StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(bytes))
                .toString()
        } catch (failure: CharacterCodingException) {
            throw TargetTransportException("C1 returned an invalid endpoint", failure)
        }
    }

    /** Must be called by the Debug bootstrap Activity when it first loses foreground eligibility. */
    fun eligibilityLost() = postToLifecycle {
        if (started && !stopped) driveAndProcess(SupervisorEvent(TransportAbi.EVENT_ELIGIBILITY_LOST))
    }

    override fun close() = runOnLifecycleAndWait {
        if (!stopped) leaseTerminal()
    }

    private fun activate() {
        check(!started && !stopped)
        val initial = supervisor.initialOutcome
        if (initial.kind != TransportAbi.OUTCOME_ENDPOINT_READY || initial.value0 != 1L || initial.value1 != 0L) {
            throw TargetTransportException("C1 returned a non-Android endpoint")
        }
        started = true
        scheduleDeadline(initial, SystemClock.elapsedRealtime())
        try {
            socketHost.start(endpointName)
        } catch (failure: Throwable) {
            internalFailure(InternalFailureOrigin.LISTENER_START_EXCEPTION)
            throw TargetTransportException("Cannot bind Android localabstract listener", failure)
        }
    }

    private fun socketEvent(event: SocketEvent) {
        if (event is SocketEvent.ReadEnded) terminalObserved += event.streamId
        postToLifecycle {
            if (!started || stopped) return@postToLifecycle
            when (event) {
                is SocketEvent.Accepted -> accepted(event.streamId)
                is SocketEvent.Bytes -> driveAndProcess(
                    SupervisorEvent(TransportAbi.EVENT_STREAM_BYTES, event.streamId, bytes = event.bytes),
                )
                is SocketEvent.ReadEnded -> driveAndProcess(
                    SupervisorEvent(
                        if (event.failed) TransportAbi.EVENT_STREAM_IO_FAILED else TransportAbi.EVENT_STREAM_EOF,
                        event.streamId,
                    ),
                )
                is SocketEvent.WriteCompleted -> {
                    val pending = pendingWrites[event.streamId]
                    if (pending?.token != event.token) return@postToLifecycle
                    wipePendingWrite(event.streamId)
                    driveAndProcess(
                        SupervisorEvent(
                            if (event.failed) TransportAbi.EVENT_STREAM_IO_FAILED else TransportAbi.EVENT_FULL_WRITE_COMMITTED,
                            event.streamId,
                            if (event.failed) 0 else event.token,
                        ),
                    )
                }
                SocketEvent.ListenerFailed -> internalFailure(InternalFailureOrigin.LISTENER_FAILED)
                SocketEvent.PeerUnsupported -> internalFailure(InternalFailureOrigin.PEER_UNSUPPORTED)
            }
        }
    }

    private fun accepted(streamId: Long) {
        if (streamId <= 0L) {
            internalFailure(InternalFailureOrigin.ACCEPTED_STREAM_INVALID)
            return
        }
        if (bootstrapStreamId == 0L) {
            bootstrapStreamId = streamId
            driveAndProcess(SupervisorEvent(TransportAbi.EVENT_BOOTSTRAP_CONNECTED, streamId))
        } else if (composition == null) {
            if (!pendingSessionAdmission.defer(streamId)) socketHost.close(streamId)
        } else {
            driveAndProcess(SupervisorEvent(TransportAbi.EVENT_SESSION_ACCEPTED, streamId))
        }
    }

    private fun driveAndProcess(event: SupervisorEvent) {
        if (stopped) {
            event.bytes.fill(0)
            return
        }
        val outcome = try {
            supervisor.drive(event)
        } catch (_: Throwable) {
            event.bytes.fill(0)
            internalFailure(InternalFailureOrigin.SUPERVISOR_DRIVE_EXCEPTION)
            return
        }
        event.bytes.fill(0)
        try {
            recordLeaseTerminalDiagnostic(event.tag, outcome)
            process(outcome, SystemClock.elapsedRealtime())
        } catch (_: Throwable) {
            internalFailure(InternalFailureOrigin.OUTCOME_PROCESSING_EXCEPTION)
        }
    }

    private fun process(outcome: SupervisorOutcome, observedAtMilliseconds: Long) {
        if (stopped) {
            outcome.bytes?.fill(0)
            return
        }
        scheduleDeadline(outcome, observedAtMilliseconds)
        when (outcome.kind) {
            // C1 uses stream_id=0 for a stale/cancelled timer no-op. It is a
            // valid outcome, not a request to read from an invalid stream.
            TransportAbi.OUTCOME_NEED_INPUT -> if (outcome.streamId > 0L) socketHost.receive(outcome.streamId)
            TransportAbi.OUTCOME_WRITE_FRAMES -> write(outcome)
            TransportAbi.OUTCOME_APPLICATION -> application(outcome)
            TransportAbi.OUTCOME_LEASE_READY -> leaseReady(outcome)
            TransportAbi.OUTCOME_SESSION_TERMINAL -> sessionTerminal(outcome.streamId)
            TransportAbi.OUTCOME_LEASE_TERMINAL, TransportAbi.OUTCOME_CLOSED -> leaseTerminal()
            else -> internalFailure(InternalFailureOrigin.OUTCOME_UNKNOWN)
        }
    }

    private fun write(outcome: SupervisorOutcome) {
        val bytes = outcome.bytes
        outcome.bytes = null
        if (outcome.streamId <= 0L || outcome.writeToken <= 0L || bytes == null || bytes.isEmpty() || pendingWrites.containsKey(outcome.streamId)) {
            bytes?.fill(0)
            internalFailure(InternalFailureOrigin.WRITE_OUTCOME_INVALID)
            return
        }
        val frameBytes = bytes
        pendingWrites[outcome.streamId] = PendingWrite(outcome.writeToken, frameBytes)
        socketHost.write(outcome.streamId, outcome.writeToken, frameBytes)
    }

    private fun application(outcome: SupervisorOutcome) {
        val request = outcome.bytes
        outcome.bytes = null
        val currentComposition = composition
        if (outcome.streamId <= 0L || request == null || request.isEmpty() || currentComposition == null) {
            request?.fill(0)
            internalFailure(InternalFailureOrigin.APPLICATION_OUTCOME_INVALID)
            return
        }
        val applicationBytes = request
        val runtime = runtimes[outcome.streamId] ?: currentComposition
            .makeRuntime(currentComposition.catalog.identity.generation)
            .also { runtimes[outcome.streamId] = it }
        val response = try {
            // No post/suspension occurs between C1 APPLICATION and ProtocolRuntime.handle.
            runtime.handle(applicationBytes)
        } catch (_: Throwable) {
            applicationBytes.fill(0)
            internalFailure(InternalFailureOrigin.APPLICATION_RUNTIME_EXCEPTION)
            return
        }
        applicationBytes.fill(0)
        if (stopped || runtimes[outcome.streamId] !== runtime || terminalObserved.contains(outcome.streamId)) {
            response.fill(0)
            return
        }
        driveAndProcess(SupervisorEvent(TransportAbi.EVENT_RUNTIME_RESPONSE, outcome.streamId, bytes = response))
        response.fill(0)
    }

    private fun leaseReady(outcome: SupervisorOutcome) {
        if (outcome.streamId != bootstrapStreamId || outcome.value0 <= 0L || outcome.value1 <= 0L || composition != null) {
            internalFailure(InternalFailureOrigin.LEASE_READY_OUTCOME_INVALID)
            return
        }
        try {
            val created = compositionFactory(outcome.value0)
            require(created.catalog.identity.generation == outcome.value0)
            composition = created
            socketHost.receive(outcome.streamId)
            pendingSessionAdmission.takeWhenReady()?.let { streamId ->
                driveAndProcess(SupervisorEvent(TransportAbi.EVENT_SESSION_ACCEPTED, streamId))
            }
        } catch (_: Throwable) {
            internalFailure(InternalFailureOrigin.LEASE_READY_COMPOSITION_EXCEPTION)
        }
    }

    private fun sessionTerminal(streamId: Long) {
        if (streamId <= 0L) return
        terminalObserved += streamId
        socketHost.close(streamId)
        wipePendingWrite(streamId)
        runtimes.remove(streamId)?.invalidateSessions()
    }

    private fun internalFailure(origin: InternalFailureOrigin) {
        if (stopped) return
        recordInternalFailureDiagnostic(origin)
        val event = SupervisorEvent(TransportAbi.EVENT_INTERNAL_ERROR)
        try {
            val outcome = supervisor.drive(event)
            recordLeaseTerminalDiagnostic(event.tag, outcome)
            process(outcome, SystemClock.elapsedRealtime())
        } catch (_: Throwable) {
            recordInternalFailureDiagnostic(InternalFailureOrigin.INTERNAL_ERROR_DRIVE_EXCEPTION)
            leaseTerminal()
        }
    }

    /** Debug-only, fixed-schema breadcrumb for the first Kotlin-side failure trigger. */
    private fun recordInternalFailureDiagnostic(origin: InternalFailureOrigin) {
        if (internalFailureDiagnosticLogged) return
        internalFailureDiagnosticLogged = true
        Log.d(
            TERMINAL_DIAGNOSTIC_TAG,
            "schema=1 stage=internal_failure origin=${origin.wireName}",
        )
    }

    /** Debug-only, fixed-schema breadcrumbs for the first C1 lease terminal. */
    private fun recordLeaseTerminalDiagnostic(eventTag: Int, outcome: SupervisorOutcome) {
        if (leaseTerminalDiagnosticLogged ||
            (outcome.kind != TransportAbi.OUTCOME_LEASE_TERMINAL && outcome.kind != TransportAbi.OUTCOME_CLOSED)
        ) return
        leaseTerminalDiagnosticLogged = true
        val stage = if (outcome.kind == TransportAbi.OUTCOME_CLOSED) "closed" else "lease_terminal"
        Log.d(
            TERMINAL_DIAGNOSTIC_TAG,
            "schema=1 stage=$stage event=${terminalEventName(eventTag)} close_reason=${closeReasonName(outcome.closeReason)}",
        )
    }

    private fun leaseTerminal() {
        if (stopped) return
        stopped = true
        pendingTimers.values.forEach { it.cancel(false) }
        pendingTimers.clear()
        socketHost.stop()
        pendingWrites.keys.toList().forEach(::wipePendingWrite)
        runtimes.values.forEach { it.invalidateSessions() }
        runtimes.clear()
        composition = null
        pendingSessionAdmission.clear()
        try {
            supervisor.close()
        } catch (_: Throwable) {
            // The local state is already stale and never reused.
        } finally {
            timers.shutdownNow()
            lifecycleThread.quitSafely()
        }
    }

    private fun scheduleDeadline(outcome: SupervisorOutcome, observedAtMilliseconds: Long) {
        val token = outcome.deadlineToken ?: return
        if (token <= 0L || outcome.nextDeadlineMilliseconds <= 0L || pendingTimers.containsKey(token)) return
        val elapsed = max(0L, SystemClock.elapsedRealtime() - observedAtMilliseconds)
        val delay = (outcome.nextDeadlineMilliseconds - elapsed).coerceAtLeast(0L)
        pendingTimers[token] = timers.schedule({
            postToLifecycle {
                pendingTimers.remove(token)
                if (!stopped) driveAndProcess(SupervisorEvent(TransportAbi.EVENT_TIMER_FIRED, writeToken = token))
            }
        }, delay, TimeUnit.MILLISECONDS)
    }

    private fun wipePendingWrite(streamId: Long) {
        pendingWrites.remove(streamId)?.bytes?.fill(0)
    }

    private fun postToLifecycle(block: () -> Unit) {
        if (!stopped) lifecycle.post(block)
    }

    private fun runOnLifecycleAndWait(block: () -> Unit) {
        if (Thread.currentThread() === lifecycleThread) return block()
        var failure: Throwable? = null
        val done = CountDownLatch(1)
        lifecycle.post {
            try {
                block()
            } catch (caught: Throwable) {
                failure = caught
            } finally {
                done.countDown()
            }
        }
        if (!done.await(2, TimeUnit.SECONDS)) throw TargetTransportException("Target transport lifecycle timed out")
        failure?.let { throw it }
    }

    private data class PendingWrite(val token: Long, val bytes: ByteArray)
}

internal class PendingSessionAdmission {
    private var streamId: Long? = null

    fun defer(streamId: Long): Boolean {
        check(streamId > 0L)
        if (this.streamId != null) return false
        this.streamId = streamId
        return true
    }

    fun takeWhenReady(): Long? = streamId.also { streamId = null }

    fun clear() {
        streamId = null
    }
}

/** A debug Activity calls this once after the app's own frozen runtime composition is ready. */
object TargetTransportBootstrap {
    @Volatile private var factory: TargetRuntimeCompositionFactory? = null
    @Volatile private var active: TargetTransport? = null

    @JvmStatic
    fun install(compositionFactory: TargetRuntimeCompositionFactory) {
        check(factory == null) { "Target transport bootstrap is already installed" }
        factory = compositionFactory
    }

    fun start(activity: Activity): TargetTransport {
        check(active == null) { "Target transport is already active" }
        val extras = activity.intent.extras
        if (extras == null || extras.keySet() != setOf(TargetTransport.DESCRIPTOR_EXTRA)) {
            throw TargetTransportException("Bootstrap Activity requires exactly one descriptor extra")
        }
        val descriptor = activity.intent.getStringExtra(TargetTransport.DESCRIPTOR_EXTRA)
            ?: throw TargetTransportException("Bootstrap descriptor is missing")
        val currentFactory = factory ?: throw TargetTransportException("Bootstrap composition is not installed")
        return TargetTransport.start(descriptor, currentFactory).also { active = it }
    }

    fun clear(transport: TargetTransport) {
        if (active === transport) active = null
    }
}

class TargetTransportBootstrapActivity : Activity() {
    private var transport: TargetTransport? = null

    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        try {
            transport = TargetTransportBootstrap.start(this)
        } catch (_: Throwable) {
            finish()
        }
    }

    override fun onPause() {
        transport?.eligibilityLost()
        super.onPause()
    }

    override fun onDestroy() {
        val current = transport
        transport = null
        if (current != null) {
            current.close()
            TargetTransportBootstrap.clear(current)
        }
        super.onDestroy()
    }
}

private sealed interface SocketEvent {
    data class Accepted(val streamId: Long) : SocketEvent
    data class Bytes(val streamId: Long, val bytes: ByteArray) : SocketEvent
    data class ReadEnded(val streamId: Long, val failed: Boolean) : SocketEvent
    data class WriteCompleted(val streamId: Long, val token: Long, val failed: Boolean) : SocketEvent
    data object ListenerFailed : SocketEvent
    data object PeerUnsupported : SocketEvent
}

internal sealed interface ReadPumpResult {
    data class Bytes(val bytes: ByteArray) : ReadPumpResult
    data class Ended(val failed: Boolean) : ReadPumpResult
}

internal class InputStreamReadPump(
    private val io: Executor,
    private val inputStreamFor: (streamId: Long) -> InputStream?,
    private val callback: (streamId: Long, result: ReadPumpResult) -> Unit,
) {
    private val readsInFlight = ConcurrentHashMap.newKeySet<Long>()

    fun receive(streamId: Long) {
        if (!readsInFlight.add(streamId)) return
        io.execute {
            val scratch = ByteArray(1_048_576)
            val result = try {
                inputStreamFor(streamId)?.let { input ->
                    val count = input.read(scratch)
                    if (count < 0) ReadPumpResult.Ended(failed = false)
                    else ReadPumpResult.Bytes(scratch.copyOf(count))
                }
            } catch (_: Throwable) {
                ReadPumpResult.Ended(failed = true)
            } finally {
                scratch.fill(0)
                readsInFlight.remove(streamId)
            }
            result?.let { callback(streamId, it) }
        }
    }

    fun stop() {
        readsInFlight.clear()
    }
}

/** Strictly AF_UNIX abstract sockets; no TCP/INET address is ever constructed in this adapter. */
private class AbstractSocketHost(private val callback: (SocketEvent) -> Unit) {
    private val io: ExecutorService = Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "apppilotkit-target-transport-io").apply { isDaemon = true }
    }
    private val nextStreamId = AtomicLong(1)
    private val streams = ConcurrentHashMap<Long, LocalSocket>()
    private val readPump = InputStreamReadPump(
        io = io,
        inputStreamFor = { streamId -> streams[streamId]?.inputStream },
    ) { streamId, result ->
        when (result) {
            is ReadPumpResult.Bytes -> callback(SocketEvent.Bytes(streamId, result.bytes))
            is ReadPumpResult.Ended -> callback(SocketEvent.ReadEnded(streamId, result.failed))
        }
    }
    private val stopped = AtomicBoolean(false)
    @Volatile private var listener: LocalServerSocket? = null

    fun start(name: String) {
        check(listener == null && !stopped.get())
        listener = LocalServerSocket(name)
        io.execute(::acceptLoop)
    }

    fun receive(streamId: Long) {
        readPump.receive(streamId)
    }

    fun write(streamId: Long, token: Long, bytes: ByteArray) {
        io.execute {
            val socket = streams[streamId]
            if (socket == null || stopped.get()) {
                callback(SocketEvent.WriteCompleted(streamId, token, true))
                return@execute
            }
            val failed = try {
                socket.outputStream.write(bytes)
                socket.outputStream.flush()
                false
            } catch (_: Throwable) {
                true
            }
            callback(SocketEvent.WriteCompleted(streamId, token, failed))
        }
    }

    fun close(streamId: Long) {
        streams.remove(streamId)?.closeQuietly()
    }

    fun stop() {
        if (!stopped.compareAndSet(false, true)) return
        listener?.closeQuietly()
        listener = null
        streams.values.forEach { it.closeQuietly() }
        streams.clear()
        readPump.stop()
        io.shutdownNow()
    }

    private fun acceptLoop() {
        while (!stopped.get()) {
            val socket = try {
                listener?.accept() ?: return
            } catch (_: Throwable) {
                if (!stopped.get()) callback(SocketEvent.ListenerFailed)
                return
            }
            val expectedShellUid = Process.SHELL_UID
            val peerUid = try {
                socket.peerCredentials.uid
            } catch (_: Throwable) {
                socket.closeQuietly()
                callback(SocketEvent.PeerUnsupported)
                return
            }
            if (peerUid != expectedShellUid) {
                socket.closeQuietly()
                // UID is only defense in depth; mismatch is unsupported, never Host identity.
                callback(SocketEvent.PeerUnsupported)
                return
            }
            val streamId = nextStreamId.getAndUpdate { current -> if (current == Long.MAX_VALUE) 0 else current + 1 }
            if (streamId <= 0L) {
                socket.closeQuietly()
                callback(SocketEvent.ListenerFailed)
                return
            }
            streams[streamId] = socket
            callback(SocketEvent.Accepted(streamId))
        }
    }
}


private fun Closeable.closeQuietly() {
    try {
        close()
    } catch (_: Throwable) {
        // A failing close is terminalized by the lifecycle owner; it is never retried/rebound.
    }
}

private fun Char.isAsciiLetterOrDigit(): Boolean =
    this in 'A'..'Z' || this in 'a'..'z' || this in '0'..'9'
