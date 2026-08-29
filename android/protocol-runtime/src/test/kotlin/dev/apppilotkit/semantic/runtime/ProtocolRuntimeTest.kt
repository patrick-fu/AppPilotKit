package dev.apppilotkit.semantic.runtime

import java.nio.charset.StandardCharsets
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicBoolean
import dev.apppilotkit.semantic.ActionPolicy
import dev.apppilotkit.semantic.AuthorizationPolicy
import dev.apppilotkit.semantic.CatalogIdentity
import dev.apppilotkit.semantic.ClassificationStatus
import dev.apppilotkit.semantic.EncodedSemanticValue
import dev.apppilotkit.semantic.RedactionStatus
import dev.apppilotkit.semantic.RetrySafety
import dev.apppilotkit.semantic.SemanticCodec
import dev.apppilotkit.semantic.SemanticRegistry
import dev.apppilotkit.semantic.SemanticRegistryBuilder
import dev.apppilotkit.semantic.SemanticSchema
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class ProtocolRuntimeTest {
    @Test
    fun `negotiation isolates semantic catalog by minor`() {
        val runtime = runtime(registry())
        val v1 = response(runtime, open(0, 1, semantic = false))
        assertEquals(listOf("session.core"), v1.resultCapabilities())
        val v2 = response(runtime, open(2, 2, semantic = true))
        assertTrue("semantic.catalog" in v2.resultCapabilities())
        assertEquals(listOf("session.core"), response(runtime, open(2, 2, semantic = false)).resultCapabilities())
        val invalidCapability = "{\"jsonrpc\":\"2.0\",\"id\":\"open-invalid\",\"method\":\"session.open\",\"params\":{\"client\":{\"name\":\"test\",\"version\":\"1\"},\"protocol\":{\"major\":1,\"minMinor\":2,\"maxMinor\":2},\"requiredCapabilities\":[\"semantic_catalog\"]}}"
        assertEquals(-32602, response(runtime, invalidCapability).errorCode())
        val context = v1.context()
        assertEquals(-32601, response(runtime, request("semantic.invoke", context, "{}")).errorCode())
        val coreV12 = response(runtime, open(2, 2, semantic = false)).context()
        assertEquals(-32003, response(runtime, request("semantic.invoke", coreV12, "{}")).errorCode())
    }

    @Test
    fun `strict envelopes and oversized request stop before resource handler`() {
        val calls = AtomicInteger()
        val runtime = runtime(registry(query = { calls.incrementAndGet(); "current" }))
        val context = response(runtime, open()).context()
        assertEquals(-32602, response(runtime, request("semantic.query", context, "{\"capability\":\"config.current\",\"declarationRevision\":1,\"valueSchema\":${handleJson(VALUE_SCHEMA)},\"extra\":true}")).errorCode())
        val oversized = request("semantic.query", context, "{\"capability\":\"config.current\",\"declarationRevision\":1,\"valueSchema\":${handleJson(VALUE_SCHEMA)},\"input\":\"${"x".repeat(1_100)}\",\"inputSchema\":${handleJson(INPUT_SCHEMA)}}")
        val oversizedResponse = response(runtime, oversized)
        assertEquals(-32004, oversizedResponse.errorCode())
        assertEquals("request", oversizedResponse["id"]!!.jsonPrimitive.content)
        assertEquals(0, calls.get())
    }

    @Test
    fun `list has no value and cursor binds session and original limits`() {
        val runtime = runtime(registry())
        val firstSession = response(runtime, open()).context()
        val first = response(runtime, request("semantic.list", firstSession, "{\"limits\":{\"maxItems\":1}}"))
        val items = first.result()["capabilities"]!!.jsonArray
        assertFalse(items.first().jsonObject.containsKey("value"))
        val cursor = first.result()["page"]!!.jsonObject["nextCursor"]!!.jsonPrimitive.content
        assertEquals(-32602, response(runtime, request("semantic.list", firstSession, "{\"cursor\":\"${cursor}mutated\"}")).errorCode())
        val secondSession = response(runtime, open()).context()
        assertEquals(-32602, response(runtime, request("semantic.list", secondSession, "{\"cursor\":\"$cursor\"}")).errorCode())
        assertEquals(1, response(runtime, request("semantic.list", firstSession, "{\"cursor\":\"$cursor\"}"))
            .result()["capabilities"]!!.jsonArray.size)
    }

    @Test
    fun `list continuation uses its initial discovery snapshot`() {
        val visible = AtomicBoolean(true)
        val runtime = runtime(
            registry(),
            SemanticProtocolPolicy(
                discover = { _, _ -> visible.get() },
                discloseSchema = { _, _ -> true },
                discloseResource = { _, _ -> true },
            ),
        )
        val context = response(runtime, open()).context()
        val first = response(runtime, request("semantic.list", context, "{\"limits\":{\"maxItems\":1}}"))
        val cursor = first.result()["page"]!!.jsonObject["nextCursor"]!!.jsonPrimitive.content
        visible.set(false)
        val continued = response(runtime, request("semantic.list", context, "{\"cursor\":\"$cursor\"}"))
        assertEquals(1, continued.result()["capabilities"]!!.jsonArray.size)
    }

    @Test
    fun `show schema unknown unavailable and policy denial remain safe`() {
        var available = false
        val policy = SemanticProtocolPolicy(
            discover = { _, declaration -> declaration.id != "hidden.config" },
            discloseSchema = { _, _ -> false },
            discloseResource = { _, _ -> false },
        )
        val runtime = runtime(registry(available = { available }), policy)
        val context = response(runtime, open()).context()
        assertEquals(-32020, response(runtime, request("semantic.show", context, "{\"capability\":\"hidden.config\",\"declarationRevision\":1}")).errorCode())
        assertEquals(-32020, response(runtime, request("semantic.show", context, "{\"capability\":\"hidden.config\",\"declarationRevision\":999}")).errorCode())
        assertEquals(-32020, response(runtime, request("semantic.show", context, "{\"capability\":\"unknown.config\",\"declarationRevision\":1}")).errorCode())
        val hiddenSchemaRuntime = runtime(
            registry(),
            SemanticProtocolPolicy(
                discover = { _, declaration -> declaration.id != "config.current" },
                discloseSchema = { _, _ -> true },
                discloseResource = { _, _ -> true },
            ),
        )
        val hiddenSchemaContext = response(hiddenSchemaRuntime, open()).context()
        assertEquals(-32020, response(hiddenSchemaRuntime, request("semantic.schema", hiddenSchemaContext, "{\"capability\":\"config.current\",\"declarationRevision\":1,\"schema\":${handleJson(INPUT_SCHEMA)}}")).errorCode())
        assertEquals(-32020, response(hiddenSchemaRuntime, request("semantic.query", hiddenSchemaContext, "{\"capability\":\"config.current\",\"declarationRevision\":1,\"valueSchema\":${handleJson(INPUT_SCHEMA)}}")).errorCode())
        assertEquals(-32023, response(runtime, request("semantic.schema", context, "{\"capability\":\"config.current\",\"declarationRevision\":1,\"schema\":${handleJson(VALUE_SCHEMA)}}")).errorCode())
        val unavailableRuntime = runtime(registry(available = { false }))
        val unavailableContext = response(unavailableRuntime, open()).context()
        assertEquals(-32022, response(unavailableRuntime, request("semantic.query", unavailableContext, queryParams())).errorCode())
        val secretParams = "{\"capability\":\"config.current\",\"declarationRevision\":1,\"inputSchema\":${handleJson(INPUT_SCHEMA)},\"input\":\"secret\",\"valueSchema\":${handleJson(VALUE_SCHEMA)}}"
        val safe = Json.encodeToString(JsonObject.serializer(), response(runtime, request("semantic.query", context, secretParams)))
        assertTrue(safe.contains("semantic.schemaMismatch"))
        assertFalse(safe.contains("secret"))
        assertEquals(-32023, response(runtime, request("semantic.query", context, queryParams())).errorCode())
    }

    @Test
    fun `query accepts resources only and invoke never reaches actions`() {
        val actionCalls = AtomicInteger()
        val runtime = runtime(registry(actionCalls = actionCalls))
        val context = response(runtime, open()).context()
        assertEquals(-32020, response(runtime, request("semantic.query", context, "{\"capability\":\"account.delete\",\"declarationRevision\":1,\"valueSchema\":${handleJson(VALUE_SCHEMA)}}")).errorCode())
        assertEquals(-32601, response(runtime, request("semantic.invoke", context, "{\"capability\":\"account.delete\",\"declarationRevision\":1,\"inputSchema\":${handleJson(INPUT_SCHEMA)},\"input\":\"secret\"}")).errorCode())
        assertEquals(0, actionCalls.get())
    }

    @Test
    fun `overflow declaration revisions are rejected without narrowing`() {
        val runtime = runtime(registry())
        val context = response(runtime, open()).context()
        val overflow = Int.MAX_VALUE.toLong() + 1
        assertEquals(-32021, response(runtime, request("semantic.show", context, "{\"capability\":\"config.current\",\"declarationRevision\":$overflow}")).errorCode())
        assertEquals(-32021, response(runtime, request("semantic.schema", context, "{\"capability\":\"config.current\",\"declarationRevision\":$overflow,\"schema\":${handleJson(VALUE_SCHEMA)}}")).errorCode())
    }

    @Test
    fun `concurrent queries do not serialize handlers and invalidation expires sessions`() {
        val calls = AtomicInteger()
        val runtime = runtime(registry(query = { calls.incrementAndGet().toString() }))
        val context = response(runtime, open()).context()
        val executor = Executors.newFixedThreadPool(8)
        try {
            val responses = (1..32).map {
                executor.submit<String> {
                    val raw = runtime.handle(request("semantic.query", context, queryParams()).toByteArray(StandardCharsets.UTF_8))
                    val envelope = Json.parseToJsonElement(String(raw, StandardCharsets.UTF_8)).jsonObject
                    envelope["result"]!!.jsonObject["value"]!!.jsonPrimitive.content
                }
            }.map { it.get(5, TimeUnit.SECONDS) }
            assertEquals((1..32).map(Int::toString).toSet(), responses.toSet())
        } finally {
            executor.shutdownNow()
        }
        runtime.invalidateSessions()
        assertEquals(-32002, response(runtime, request("semantic.list", context, "{}")).errorCode())
    }

    @Test
    fun `invalidation after a read starts prevents its disclosure`() {
        val started = CountDownLatch(1)
        val release = CountDownLatch(1)
        val runtime = runtime(registry(query = {
            started.countDown()
            release.await(5, TimeUnit.SECONDS)
            "completed"
        }))
        val context = response(runtime, open()).context()
        val executor = Executors.newSingleThreadExecutor()
        try {
            val result = executor.submit<JsonObject> {
                response(runtime, request("semantic.query", context, queryParams()))
            }
            assertTrue(started.await(5, TimeUnit.SECONDS))
            runtime.invalidateSessions()
            release.countDown()
            assertEquals(-32002, result.get(5, TimeUnit.SECONDS).errorCode())
        } finally {
            release.countDown()
            executor.shutdownNow()
        }
    }

    private fun runtime(
        catalog: SemanticRegistry,
        policy: SemanticProtocolPolicy = allowAllPolicy(),
    ) = ProtocolRuntime(catalog, ProtocolRuntimeLimits(1_024, 4_096, 4), policy)

    private fun registry(
        query: () -> String = { "current" },
        available: () -> Boolean = { true },
        actionCalls: AtomicInteger = AtomicInteger(),
    ): SemanticRegistry = SemanticRegistryBuilder()
        .registerResource("config.current", 1, StringCodec(VALUE_SCHEMA), available, query)
        .registerResource("hidden.config", 1, StringCodec(VALUE_SCHEMA)) { "hidden" }
        .registerAction(
            "account.delete",
            1,
            StringCodec(INPUT_SCHEMA),
            ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
        ) { actionCalls.incrementAndGet() }
        .freeze(CatalogIdentity("catalog_runtimefixture", 7), 4_096)

    private fun allowAllPolicy() = SemanticProtocolPolicy(
        discover = { _, _ -> true },
        discloseSchema = { _, _ -> true },
        discloseResource = { _, _ -> true },
    )

    private fun open(minMinor: Int = 2, maxMinor: Int = 2, semantic: Boolean = true): String =
        "{\"jsonrpc\":\"2.0\",\"id\":\"open\",\"method\":\"session.open\",\"params\":{\"client\":{\"name\":\"test\",\"version\":\"1\"},\"protocol\":{\"major\":1,\"minMinor\":$minMinor,\"maxMinor\":$maxMinor}${if (semantic) ",\"requiredCapabilities\":[\"semantic.catalog\"]" else ""}}}"

    private fun request(method: String, context: JsonObject, params: String): String =
        "{\"jsonrpc\":\"2.0\",\"id\":\"request\",\"method\":\"$method\",\"context\":${Json.encodeToString(JsonObject.serializer(), context)},\"params\":$params}"

    private fun response(runtime: ProtocolRuntime, request: String): JsonObject =
        Json.parseToJsonElement(String(runtime.handle(request.toByteArray(StandardCharsets.UTF_8)), StandardCharsets.UTF_8)).jsonObject

    private fun queryParams() = "{\"capability\":\"config.current\",\"declarationRevision\":1,\"valueSchema\":${handleJson(VALUE_SCHEMA)}}"
    private fun handleJson(schema: SemanticSchema) = Json.encodeToString(JsonObject.serializer(), buildJsonObject {
        put("id", schema.handle.id)
        put("revision", schema.handle.revision)
        put("digest", schema.handle.digest)
    })

    private fun JsonObject.context() = result()["context"]!!.jsonObject
    private fun JsonObject.result() = this["result"]!!.jsonObject
    private fun JsonObject.resultCapabilities() = result()["capabilities"]!!.jsonArray.map { it.jsonPrimitive.content }
    private fun JsonObject.errorCode() = this["error"]!!.jsonObject["code"]!!.jsonPrimitive.content.toInt()

    private class StringCodec(override val schema: SemanticSchema) : SemanticCodec<String> {
        override fun decode(value: JsonElement): String = value.jsonPrimitive.content
        override fun encode(value: String) = EncodedSemanticValue(
            Json.encodeToString(JsonPrimitive.serializer(), JsonPrimitive(value)).toByteArray(StandardCharsets.UTF_8),
            schema.handle,
            ClassificationStatus.COMPLETE,
            RedactionStatus.COMPLETE,
        )
        override fun validates(value: JsonElement): Boolean = value is JsonPrimitive && value.isString
    }

    private companion object {
        val VALUE_SCHEMA = schema("schema_value_runtime01", "app://fixture/runtime-value@1")
        val INPUT_SCHEMA = schema("schema_input_runtime01", "app://fixture/runtime-input@1")
        fun schema(id: String, uri: String): SemanticSchema = SemanticSchema.create(id, 1, buildJsonObject {
            put("\$schema", "https://json-schema.org/draft/2020-12/schema")
            put("\$id", uri)
            put("type", "string")
        })
    }
}
