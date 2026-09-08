package dev.apppilotkit.targettransport.internal

import java.io.IOException
import java.io.InputStream
import java.util.ArrayDeque
import java.util.concurrent.Executor
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class InputStreamReadPumpTest {
    @Test
    fun `callback can immediately schedule the next read`() {
        val executor = ImmediateExecutor()
        val input = ScriptedInputStream(byteArrayOf(1), byteArrayOf(2))
        val results = mutableListOf<ReadPumpResult>()
        lateinit var pump: InputStreamReadPump
        pump = InputStreamReadPump(executor, { input }) { streamId, result ->
            results += result
            if (results.size == 1) pump.receive(streamId)
        }

        pump.receive(7)

        assertEquals(2, input.readCalls)
        assertEquals(2, results.size)
        assertArrayEquals(byteArrayOf(1), (results[0] as ReadPumpResult.Bytes).bytes)
        assertArrayEquals(byteArrayOf(2), (results[1] as ReadPumpResult.Bytes).bytes)
    }

    @Test
    fun `duplicate receives before a read runs issue one read`() {
        val executor = QueuedExecutor()
        val input = ScriptedInputStream(byteArrayOf(3))
        val results = mutableListOf<ReadPumpResult>()
        val pump = InputStreamReadPump(executor, { input }) { _, result -> results += result }

        pump.receive(7)
        pump.receive(7)
        executor.runNext()

        assertEquals(1, input.readCalls)
        assertEquals(1, results.size)
    }

    @Test
    fun `eof is reported without failure after one read`() {
        val input = ScriptedInputStream(endOfStream = true)
        val results = mutableListOf<ReadPumpResult>()
        val pump = InputStreamReadPump(ImmediateExecutor(), { input }) { _, result -> results += result }

        pump.receive(7)

        assertEquals(1, input.readCalls)
        assertEquals(listOf(ReadPumpResult.Ended(failed = false)), results)
    }

    @Test
    fun `input failure is reported as a failed end`() {
        val input = ScriptedInputStream(failure = IOException("read failed"))
        val results = mutableListOf<ReadPumpResult>()
        val pump = InputStreamReadPump(ImmediateExecutor(), { input }) { _, result -> results += result }

        pump.receive(7)

        assertEquals(1, input.readCalls)
        assertEquals(listOf(ReadPumpResult.Ended(failed = true)), results)
    }

    @Test
    fun `input lookup failure is reported as a failed end`() {
        var lookups = 0
        val results = mutableListOf<ReadPumpResult>()
        val pump = InputStreamReadPump(ImmediateExecutor(), {
            lookups += 1
            throw IOException("input unavailable")
        }) { _, result -> results += result }

        pump.receive(7)

        assertEquals(1, lookups)
        assertEquals(listOf(ReadPumpResult.Ended(failed = true)), results)
    }

    private class ImmediateExecutor : Executor {
        override fun execute(command: Runnable) = command.run()
    }

    private class QueuedExecutor : Executor {
        private val commands = ArrayDeque<Runnable>()

        override fun execute(command: Runnable) {
            commands.addLast(command)
        }

        fun runNext() {
            assertTrue(commands.isNotEmpty())
            commands.removeFirst().run()
        }
    }

    private class ScriptedInputStream(
        vararg chunks: ByteArray,
        private val endOfStream: Boolean = false,
        private val failure: IOException? = null,
    ) : InputStream() {
        private val chunks = ArrayDeque(chunks.toList())
        var readCalls = 0
            private set

        override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
            readCalls += 1
            failure?.let { throw it }
            if (endOfStream) return -1
            if (chunks.isEmpty()) return -1
            val next = chunks.removeFirst()
            next.copyInto(buffer, destinationOffset = offset, startIndex = 0, endIndex = minOf(next.size, length))
            return minOf(next.size, length)
        }

        override fun read(): Int = error("read(byte[], offset, length) should be used")
    }
}
