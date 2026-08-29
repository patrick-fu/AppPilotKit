package dev.apppilotkit.semantic

import java.nio.charset.StandardCharsets
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonObject
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertTrue
import kotlin.test.fail

class SemanticRegistryTest {
    @Test
    fun `freeze preserves membership and rejects later registration`() {
        val valueCodec = objectCodec(
            "schema_value0001",
            "app://fixture/value@1",
            "mode" to "string",
        )
        val inputCodec = stringCodec("schema_input0001", "app://fixture/input@1")
        val builder = SemanticRegistryBuilder()
            .registerResource("config.current", 1, valueCodec) {
                buildJsonObject { put("mode", "safe") }
            }

        val duplicate = expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            builder.registerAction(
                id = "config.current",
                declarationRevision = 1,
                inputCodec = inputCodec,
                policy = ActionPolicy(
                    AuthorizationPolicy.NONE,
                    RetrySafety.NO_AUTOMATIC_RETRY,
                ),
            ) { }
        }
        assertEquals(SemanticFailureKind.INVALID_REGISTRATION.stockMessage, duplicate.message)

        val registry = builder.freeze(CatalogIdentity("catalog_process0001", 1), 1_024)
        assertEquals(listOf("config.current"), registry.list().map { it.id })
        expectFailure(SemanticFailureKind.CATALOG_FROZEN) {
            builder.registerResource("config.next", 1, valueCodec) {
                buildJsonObject { put("mode", "next") }
            }
        }
        assertEquals(listOf("config.current"), registry.list().map { it.id })
    }

    @Test
    fun `invalid IDs schemas codecs and freeze metadata fail without partial registration`() {
        val validCodec = stringCodec("schema_value0002", "app://fixture/string@1")
        val invalidCodec = StringCodec(
            schema = schema("schema_value0003", "app://fixture/invalid-codec@1", "string"),
            registrationValid = false,
        )
        val builder = SemanticRegistryBuilder()

        expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            builder.registerResource("Invalid ID", 1, validCodec) { "value" }
        }
        expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            builder.registerResource("invalid.revision", 0, validCodec) { "value" }
        }
        expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            builder.registerResource("invalid.codec", 1, invalidCodec) { "value" }
        }
        expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            SemanticSchema.create(
                "schema_invalid1",
                1,
                buildJsonObject {
                    put("\$schema", "https://json-schema.org/draft/2019-09/schema")
                    put("\$id", "app://fixture/invalid")
                },
            )
        }
        expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            schema("schema_invalid2", "app://fixture/invalid-type", "not-a-type")
        }
        expectFailure(SemanticFailureKind.INVALID_REGISTRATION) {
            builder.freeze(CatalogIdentity("bad", 0), 512)
        }

        builder.registerResource("valid.after_failures", 1, validCodec) { "value" }
        val registry = builder.freeze(CatalogIdentity("catalog_process0002", 2), 512)
        assertEquals(listOf("valid.after_failures"), registry.list().map { it.id })
    }

    @Test
    fun `availability and resource values are evaluated for every query`() {
        var available = false
        var value = 0
        val codec = objectCodec(
            "schema_value0004",
            "app://fixture/counter@1",
            "value" to "integer",
        )
        val registry = SemanticRegistryBuilder()
            .registerResource(
                id = "counter.current",
                declarationRevision = 1,
                valueCodec = codec,
                available = { available },
            ) {
                buildJsonObject { put("value", ++value) }
            }
            .freeze(CatalogIdentity("catalog_process0003", 3), 1_024)
        val request = ResourceQuery(
            capability = "counter.current",
            declarationRevision = 1,
            valueSchema = codec.schema.handle,
        )

        expectFailure(SemanticFailureKind.UNAVAILABLE) { registry.query(request) }
        available = true
        val first = registry.query(request)
        val second = registry.query(request)

        assertEquals(1, first.value.jsonObject["value"]?.jsonPrimitive?.content?.toInt())
        assertEquals(2, second.value.jsonObject["value"]?.jsonPrimitive?.content?.toInt())
        assertEquals(Json.encodeToString(JsonElement.serializer(), first.value).toByteArray().size, first.bytes)
        assertEquals(1, registry.list().size)
    }

    @Test
    fun `frozen registry supports concurrent read invocations`() {
        val counter = AtomicInteger()
        val valueCodec = stringCodec("schema_value0013", "app://fixture/concurrent@1")
        val registry = SemanticRegistryBuilder()
            .registerResource("counter.concurrent", 1, valueCodec) {
                counter.incrementAndGet().toString()
            }
            .freeze(CatalogIdentity("catalog_process0013", 13), 128)
        val query = ResourceQuery(
            capability = "counter.concurrent",
            declarationRevision = 1,
            valueSchema = valueCodec.schema.handle,
        )
        val executor = Executors.newFixedThreadPool(8)

        try {
            val results = (1..50)
                .map { executor.submit<String> { registry.query(query).value.jsonPrimitive.content } }
                .map { it.get(5, TimeUnit.SECONDS) }
            assertEquals((1..50).map(Int::toString).toSet(), results.toSet())
            assertEquals(listOf("counter.concurrent"), registry.list().map { it.id })
        } finally {
            executor.shutdownNow()
        }
    }

    @Test
    fun `query decodes strongly typed input and schema mismatch never invokes handler`() {
        val inputCodec = stringCodec("schema_input0005", "app://fixture/key@1")
        val outputCodec = stringCodec("schema_value0005", "app://fixture/result@1")
        var observed: String? = null
        var availabilityChecks = 0
        val registry = SemanticRegistryBuilder()
            .registerResource(
                id = "lookup.value",
                declarationRevision = 4,
                inputCodec = inputCodec,
                valueCodec = outputCodec,
                available = { availabilityChecks += 1; true },
            ) { input ->
                observed = input
                "result-$input"
            }
            .freeze(CatalogIdentity("catalog_process0004", 4), 1_024)

        val mismatch = ResourceQuery(
            capability = "lookup.value",
            declarationRevision = 4,
            inputSchema = outputCodec.schema.handle,
            input = JsonPrimitive("secret-key"),
            valueSchema = outputCodec.schema.handle,
        )
        val failure = expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) { registry.query(mismatch) }
        assertEquals(null, observed)
        assertEquals(0, availabilityChecks)
        assertFalse(failure.message.orEmpty().contains("secret-key"))

        expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            registry.query(
                mismatch.copy(
                    inputSchema = inputCodec.schema.handle,
                    input = JsonPrimitive(7),
                ),
            )
        }
        assertEquals(null, observed)
        assertEquals(0, availabilityChecks)

        val result = registry.query(
            mismatch.copy(inputSchema = inputCodec.schema.handle),
        )
        assertEquals("secret-key", observed)
        assertEquals(1, availabilityChecks)
        assertEquals(JsonPrimitive("result-secret-key"), result.value)
    }

    @Test
    fun `disclosure rejects every unsafe output atomically with stock errors`() {
        val declared = objectSchema(
            "schema_value0006",
            "app://fixture/disclosure@1",
            "value" to "string",
        )
        val other = schema("schema_value0007", "app://fixture/other@1", "object")
        val unsafeCases = listOf(
            "unclassified" to EncodedSemanticValue(
                "{}".toByteArray(),
                declared.handle,
                ClassificationStatus.UNCLASSIFIED,
                RedactionStatus.COMPLETE,
            ),
            "incomplete-redaction" to EncodedSemanticValue(
                "{}".toByteArray(),
                declared.handle,
                ClassificationStatus.COMPLETE,
                RedactionStatus.INCOMPLETE,
            ),
            "invalid-json" to EncodedSemanticValue(
                "not-json".toByteArray(),
                declared.handle,
                ClassificationStatus.COMPLETE,
                RedactionStatus.COMPLETE,
            ),
            "invalid-utf8" to EncodedSemanticValue(
                byteArrayOf(0xC3.toByte(), 0x28),
                declared.handle,
                ClassificationStatus.COMPLETE,
                RedactionStatus.COMPLETE,
            ),
            "undeclared-schema" to EncodedSemanticValue(
                "{}".toByteArray(),
                other.handle,
                ClassificationStatus.COMPLETE,
                RedactionStatus.COMPLETE,
            ),
        )

        for ((idSuffix, payload) in unsafeCases) {
            val codec = PayloadCodec(declared) { payload }
            val registry = SemanticRegistryBuilder()
                .registerResource("unsafe.$idSuffix", 1, codec, query = { })
                .freeze(CatalogIdentity("catalog_${idSuffix.replace("-", "_")}00000000", 1), 128)
            val failure = try {
                registry.query(ResourceQuery("unsafe.$idSuffix", 1, valueSchema = declared.handle))
                fail("$idSuffix should be rejected")
            } catch (failure: SemanticFailure) {
                failure
            }
            assertTrue(
                failure.kind == SemanticFailureKind.DISCLOSURE_DENIED ||
                    failure.kind == SemanticFailureKind.SCHEMA_MISMATCH,
            )
            assertFalse(failure.message.orEmpty().contains(idSuffix))
        }

        val invalidByDeclaredCodec = PayloadCodec(
            declared,
            validate = { false },
        ) {
            safePayload(declared, "{}")
        }
        val invalidRegistry = SemanticRegistryBuilder()
            .registerResource("unsafe.invalid", 1, invalidByDeclaredCodec, query = { })
            .freeze(CatalogIdentity("catalog_invalid00000000", 1), 128)
        expectFailure(SemanticFailureKind.DISCLOSURE_DENIED) {
            invalidRegistry.query(ResourceQuery("unsafe.invalid", 1, valueSchema = declared.handle))
        }

        val schemaInvalidCodec = PayloadCodec(declared, validate = { true }) {
            safePayload(declared, "{\"value\":7}")
        }
        val schemaInvalidRegistry = SemanticRegistryBuilder()
            .registerResource("unsafe.schema_invalid", 1, schemaInvalidCodec, query = { })
            .freeze(CatalogIdentity("catalog_schemainvalid000", 1), 128)
        expectFailure(SemanticFailureKind.DISCLOSURE_DENIED) {
            schemaInvalidRegistry.query(
                ResourceQuery("unsafe.schema_invalid", 1, valueSchema = declared.handle),
            )
        }

        val oversizedCodec = PayloadCodec(declared) {
            safePayload(declared, "{\"value\":\"${"x".repeat(128)}\"}")
        }
        val oversizedRegistry = SemanticRegistryBuilder()
            .registerResource("unsafe.oversized", 1, oversizedCodec, query = { })
            .freeze(CatalogIdentity("catalog_oversized000000", 1), 32)
        expectFailure(SemanticFailureKind.RESOURCE_EXHAUSTED) {
            oversizedRegistry.query(
                ResourceQuery("unsafe.oversized", 1, valueSchema = declared.handle),
            )
        }
    }

    @Test
    fun `typed input and handler failures cannot enter safe error text`() {
        val inputCodec = StringCodec(
            schema = schema("schema_input0008", "app://fixture/secret@1", "string"),
            decodeBlock = { throw IllegalArgumentException("typed-secret") },
        )
        val outputCodec = stringCodec("schema_value0008", "app://fixture/output@1")
        val registry = SemanticRegistryBuilder()
            .registerResource("secret.lookup", 1, inputCodec, outputCodec) { "unused-$it" }
            .freeze(CatalogIdentity("catalog_process0008", 8), 1_024)

        val failure = expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            registry.query(
                ResourceQuery(
                    capability = "secret.lookup",
                    declarationRevision = 1,
                    inputSchema = inputCodec.schema.handle,
                    input = JsonPrimitive("typed-secret"),
                    valueSchema = outputCodec.schema.handle,
                ),
            )
        }
        assertEquals(SemanticFailureKind.SCHEMA_MISMATCH.stockMessage, failure.message)
        assertFalse(failure.message.orEmpty().contains("typed-secret"))
        assertEquals(null, failure.cause)
    }

    @Test
    fun `action preparation is typed dynamic and dispatch remains a separate seam`() {
        var available = false
        var availabilityChecks = 0
        var dispatched: String? = null
        val inputCodec = stringCodec("schema_input0009", "app://fixture/action@1")
        val registry = SemanticRegistryBuilder()
            .registerAction(
                id = "cache.clear",
                declarationRevision = 2,
                inputCodec = inputCodec,
                policy = ActionPolicy(
                    AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION,
                    RetrySafety.NO_AUTOMATIC_RETRY,
                ),
                available = { availabilityChecks += 1; available },
            ) { input -> dispatched = input }
            .freeze(CatalogIdentity("catalog_process0009", 9), 1_024)
        val invocation = ActionInvocation(
            capability = "cache.clear",
            declarationRevision = 2,
            inputSchema = inputCodec.schema.handle,
            input = JsonPrimitive("all"),
        )

        expectFailure(SemanticFailureKind.UNAVAILABLE) { registry.prepareAction(invocation) }
        assertEquals(1, availabilityChecks)
        expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            registry.prepareAction(invocation.copy(inputSchema = schema(
                "schema_wrong0009",
                "app://fixture/wrong@1",
                "string",
            ).handle))
        }
        assertEquals(1, availabilityChecks)
        expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            registry.prepareAction(invocation.copy(input = JsonPrimitive(42)))
        }
        assertEquals(1, availabilityChecks)
        available = true
        assertEquals(null, dispatched)
        val prepared = registry.prepareAction(invocation)
        assertEquals(null, dispatched)
        assertIs<ActionDeclaration>(prepared.declaration)
        prepared.dispatch()
        assertEquals("all", dispatched)
    }

    @Test
    fun `declarations and schema retrieval expose only frozen static metadata`() {
        val valueCodec = stringCodec("schema_value0010", "app://fixture/static@1")
        val registry = SemanticRegistryBuilder()
            .registerResource("static.value", 3, valueCodec) { "live-business-value" }
            .freeze(CatalogIdentity("catalog_process0010", 10), 1_024)

        val declaration = assertIs<ResourceDeclaration>(registry.show("static.value", 3))
        assertEquals(valueCodec.schema.handle, declaration.valueSchema)
        assertFalse(
            JsonPrimitive("live-business-value") ==
                registry.schema("static.value", 3, declaration.valueSchema),
        )
        assertEquals(
            "app://fixture/static@1",
            registry.schema("static.value", 3, declaration.valueSchema)["\$id"]
                ?.jsonPrimitive
                ?.content,
        )
        expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            registry.schema("static.value", 2, declaration.valueSchema)
        }
    }

    @Test
    fun `schema snapshot is deep frozen and output bytes use canonical JSON`() {
        val nestedProperties = mutableMapOf<String, JsonElement>(
            "value" to buildJsonObject { put("type", "string") },
        )
        val root = mutableMapOf<String, JsonElement>(
            "\$schema" to JsonPrimitive("https://json-schema.org/draft/2020-12/schema"),
            "\$id" to JsonPrimitive("app://fixture/deep-frozen@1"),
            "type" to JsonPrimitive("object"),
            "properties" to JsonObject(nestedProperties),
            "additionalProperties" to JsonPrimitive(false),
        )
        val frozenSchema = SemanticSchema.create("schema_value0011", 1, JsonObject(root))
        val digest = frozenSchema.handle.digest
        val reorderedSchema = SemanticSchema.create(
            "schema_value0011",
            1,
            buildJsonObject {
                put("type", "object")
                put("additionalProperties", false)
                putJsonObject("properties") {
                    putJsonObject("value") { put("type", "string") }
                }
                put("\$id", "app://fixture/deep-frozen@1")
                put("\$schema", "https://json-schema.org/draft/2020-12/schema")
            },
        )
        nestedProperties["injected"] = buildJsonObject { put("type", "string") }
        root["description"] = JsonPrimitive("mutated")

        assertEquals(digest, frozenSchema.handle.digest)
        assertEquals(digest, reorderedSchema.handle.digest)
        assertFalse("injected" in (frozenSchema.document["properties"] as JsonObject))
        assertFalse("description" in frozenSchema.document)

        val whitespaceCodec = PayloadCodec(frozenSchema) {
            safePayload(frozenSchema, "{ \"value\" : \"safe\" }")
        }
        val registry = SemanticRegistryBuilder()
            .registerResource("canonical.value", 1, whitespaceCodec, query = { })
            .freeze(CatalogIdentity("catalog_process0011", 11), 1_024)
        val result = registry.query(
            ResourceQuery("canonical.value", 1, valueSchema = frozenSchema.handle),
        )
        assertEquals("{\"value\":\"safe\"}".toByteArray().size, result.bytes)
    }

    @Test
    fun `query bytes match JSON stringify number normalization`() {
        val numberSchema = schema("schema_value0012", "app://fixture/number@1", "number")

        fun queryNumber(id: String, rawNumber: String): ResourceQueryResult {
            val codec = PayloadCodec(numberSchema, validate = { true }) {
                safePayload(numberSchema, rawNumber)
            }
            val registry = SemanticRegistryBuilder()
                .registerResource(id, 1, codec, query = { })
                .freeze(CatalogIdentity("catalog_${id.replace('.', '_')}00000000", 12), 128)
            return registry.query(ResourceQuery(id, 1, valueSchema = numberSchema.handle))
        }

        val exponent = queryNumber("number.exponent", "1e+2")
        assertEquals(JsonPrimitive(100), exponent.value)
        assertEquals("100".toByteArray().size, exponent.bytes)
        val negativeZero = queryNumber("number.negative_zero", "-0")
        assertEquals(JsonPrimitive(0), negativeZero.value)
        assertEquals("0".toByteArray().size, negativeZero.bytes)
        val small = queryNumber("number.small", "0.0000001")
        assertEquals(Json.parseToJsonElement("1e-7"), small.value)
        assertEquals("1e-7".toByteArray().size, small.bytes)
        val large = queryNumber("number.large", "1000000000000000000000")
        assertEquals(Json.parseToJsonElement("1e+21"), large.value)
        assertEquals("1e+21".toByteArray().size, large.bytes)
        val wideInteger = queryNumber("number.wide_integer", "999999999999999999")
        assertEquals(Json.parseToJsonElement("1000000000000000000"), wideInteger.value)
        assertEquals("1000000000000000000".toByteArray().size, wideInteger.bytes)
    }

    @Test
    fun `string schema length uses Unicode code points`() {
        val codePointSchema = SemanticSchema.create(
            "schema_codepoints01",
            1,
            buildJsonObject {
                put("\$schema", "https://json-schema.org/draft/2020-12/schema")
                put("\$id", "app://fixture/code-points@1")
                put("type", "string")
                put("minLength", 2)
                put("maxLength", 2)
            },
        )
        val codec = StringCodec(codePointSchema)
        val registry = SemanticRegistryBuilder()
            .registerResource("string.codepoints", 1, codec) { "e\u0301" }
            .freeze(CatalogIdentity("catalog_codepoints000", 14), 128)

        assertEquals(
            JsonPrimitive("e\u0301"),
            registry.query(ResourceQuery("string.codepoints", 1, valueSchema = codePointSchema.handle)).value,
        )
    }
}

private fun schema(
    id: String,
    uri: String,
    type: String,
    vararg properties: Pair<String, String>,
): SemanticSchema = SemanticSchema.create(
    id = id,
    revision = 1,
    document = buildJsonObject {
        put("\$schema", "https://json-schema.org/draft/2020-12/schema")
        put("\$id", uri)
        put("type", type)
        if (type == "object") {
            putJsonObject("properties") {
                properties.forEach { (name, propertyType) ->
                    putJsonObject(name) { put("type", propertyType) }
                }
            }
            put("additionalProperties", false)
        }
    },
)

private fun objectSchema(
    id: String,
    uri: String,
    vararg properties: Pair<String, String>,
): SemanticSchema = schema(id, uri, "object", *properties)

private fun stringCodec(id: String, uri: String): StringCodec = StringCodec(
    schema = schema(id, uri, "string"),
)

private fun objectCodec(
    id: String,
    uri: String,
    vararg properties: Pair<String, String>,
): JsonObjectCodec = JsonObjectCodec(
    schema = objectSchema(id, uri, *properties),
)

private class StringCodec(
    override val schema: SemanticSchema,
    private val registrationValid: Boolean = true,
    private val decodeBlock: (JsonElement) -> String = { it.jsonPrimitive.content },
) : SemanticCodec<String> {
    override fun isRegistrationValid(): Boolean = registrationValid

    override fun decode(value: JsonElement): String = decodeBlock(value)

    override fun encode(value: String): EncodedSemanticValue = safePayload(
        schema,
        Json.encodeToString(JsonElement.serializer(), JsonPrimitive(value)),
    )

    override fun validates(value: JsonElement): Boolean = value is JsonPrimitive && value.isString
}

private class JsonObjectCodec(
    override val schema: SemanticSchema,
) : SemanticCodec<JsonObject> {
    override fun decode(value: JsonElement): JsonObject = value.jsonObject

    override fun encode(value: JsonObject): EncodedSemanticValue = safePayload(
        schema,
        Json.encodeToString(JsonElement.serializer(), value),
    )

    override fun validates(value: JsonElement): Boolean = value is JsonObject
}

private class PayloadCodec(
    override val schema: SemanticSchema,
    private val validate: (JsonElement) -> Boolean = { it is JsonObject },
    private val payload: () -> EncodedSemanticValue,
) : SemanticCodec<Unit> {
    override fun decode(value: JsonElement) = Unit

    override fun encode(value: Unit): EncodedSemanticValue = payload()

    override fun validates(value: JsonElement): Boolean = validate(value)
}

private fun safePayload(schema: SemanticSchema, json: String): EncodedSemanticValue =
    EncodedSemanticValue(
        utf8 = json.toByteArray(StandardCharsets.UTF_8),
        schema = schema.handle,
        classification = ClassificationStatus.COMPLETE,
        redaction = RedactionStatus.COMPLETE,
    )

private fun expectFailure(
    kind: SemanticFailureKind,
    operation: () -> Unit,
): SemanticFailure = try {
    operation()
    fail("Expected $kind")
} catch (failure: SemanticFailure) {
    assertEquals(kind, failure.kind)
    failure
}
