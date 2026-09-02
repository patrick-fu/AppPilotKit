package dev.apppilotkit.transport

import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The only Java/Kotlin shape the C1 JNI_OnLoad registers against. This class is
 * deliberately in the debug source set: Release has neither a load site nor a
 * DEX class for the Rust transport library.
 */
internal object NativeTransport {
    private val loaded = AtomicBoolean(false)

    fun ensureLoaded() {
        if (loaded.compareAndSet(false, true)) {
            try {
                System.loadLibrary("apppilotkit_transport")
            } catch (failure: Throwable) {
                loaded.set(false)
                throw failure
            }
        }
    }

    @JvmStatic external fun abiVersion(): Int
    @JvmStatic external fun create(descriptor: ByteBuffer, descriptorLen: Long, outcome: ByteBuffer): Long
    @JvmStatic external fun drive(
        handle: Long,
        tag: Int,
        flags: Int,
        streamId: Long,
        writeToken: Long,
        bytes: ByteBuffer?,
        bytesLen: Long,
        outcome: ByteBuffer,
    ): Int

    @JvmStatic external fun close(handle: Long, outcome: ByteBuffer): Int
    @JvmStatic external fun drop(handle: Long): Int
    @JvmStatic external fun outputCount(handle: Long): Long
    @JvmStatic external fun outputLen(output: Long): Long
    @JvmStatic external fun outputCopy(output: Long, destination: ByteBuffer, capacity: Long): Long
    @JvmStatic external fun outputDrop(output: Long): Int
}
