package dev.apppilotkit.semantic

import java.nio.charset.StandardCharsets
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
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
    fun `action coordinator prepares typed input and keeps handler dispatch private`() {
        var available = false
        var availabilityChecks = 0
        var policyChecks = 0
        var beforeEvidence = 0
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
        val grantConsumed = AtomicBoolean(false)
        val grants = TwoPhaseGrantStore("grant", grantConsumed)

        val coordinator = registry.targetActionCoordinator(
            targetId = "target_registry_test",
            policyResolver = EffectiveActionPolicyResolver { _, declaration ->
                policyChecks += 1
                EffectiveActionPolicy(declaration.authorization, declaration.retrySafety)
            },
            destructiveAuthorizationValidator = grants,
            evidence = object : ActionEvidencePort {
                override fun captureBefore(context: TargetActionContext) {
                    beforeEvidence += 1
                }
                override fun observeStability(context: TargetActionContext) = Unit
                override fun captureAfter(context: TargetActionContext) = Unit
            },
        )
        fun request(action: ActionInvocation) = TargetActionRequest(
            invocation = action,
            context = TargetActionContext("target_registry_test", 9, "session_registry_test"),
            authorizationGrant = "grant",
            sessionIsActive = { true },
        )

        expectFailure(SemanticFailureKind.UNAVAILABLE) { coordinator.invoke(request(invocation)) }
        assertEquals(1, availabilityChecks)
        assertEquals(0, policyChecks)
        assertEquals(0, grants.validated.size)
        assertEquals(0, grants.consumeAttempts.size)
        assertEquals(0, beforeEvidence)
        expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            coordinator.invoke(request(invocation.copy(inputSchema = schema(
                "schema_wrong0009",
                "app://fixture/wrong@1",
                "string",
            ).handle)))
        }
        assertEquals(1, availabilityChecks)
        assertEquals(0, policyChecks)
        assertEquals(0, grants.validated.size)
        assertEquals(0, grants.consumeAttempts.size)
        assertEquals(0, beforeEvidence)
        expectFailure(SemanticFailureKind.SCHEMA_MISMATCH) {
            coordinator.invoke(request(invocation.copy(input = JsonPrimitive(42))))
        }
        assertEquals(1, availabilityChecks)
        assertEquals(0, policyChecks)
        assertEquals(0, grants.validated.size)
        assertEquals(0, grants.consumeAttempts.size)
        assertEquals(0, beforeEvidence)
        available = true
        assertEquals(null, dispatched)
        assertEquals(TargetActionResult.COMPLETED, coordinator.invoke(request(invocation)))
        assertEquals("all", dispatched)
        assertEquals(1, policyChecks)
        assertEquals(1, grants.validated.size)
        assertEquals(1, grants.consumeAttempts.size)
        assertEquals(1, beforeEvidence)
        assertEquals(
            TargetActionFailureKind.POLICY_DENIED,
            runCatching { coordinator.invoke(request(invocation)) }.exceptionOrNull()
                .let { it as? TargetActionFailure }?.kind,
        )
        assertEquals("all", dispatched)
    }

    @Test
    fun `policy mismatch denies before grant writer evidence and handler`() {
        val input = stringCodec("schema_input0016", "app://fixture/policy@1")
        var policyChecks = 0
        var beforeEvidence = 0
        var dispatched = 0
        val grants = TwoPhaseGrantStore("grant")
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "policy.denied",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { dispatched += 1 }
            .freeze(CatalogIdentity("catalog_process0016", 16), 1_024)
        val coordinator = registry.targetActionCoordinator(
            targetId = "target_policy_test",
            policyResolver = EffectiveActionPolicyResolver { _, _ ->
                policyChecks += 1
                EffectiveActionPolicy(AuthorizationPolicy.NONE, RetrySafety.RETRY_WITH_PROOF_ONLY)
            },
            destructiveAuthorizationValidator = grants,
            evidence = object : ActionEvidencePort {
                override fun captureBefore(context: TargetActionContext) {
                    beforeEvidence += 1
                }
                override fun observeStability(context: TargetActionContext) = Unit
                override fun captureAfter(context: TargetActionContext) = Unit
            },
        )

        val failure = runCatching {
            coordinator.invoke(
                TargetActionRequest(
                    ActionInvocation("policy.denied", 1, input.schema.handle, JsonPrimitive("go")),
                    TargetActionContext("target_policy_test", 16, "session_policy_test"),
                    "grant",
                    { true },
                ),
            )
        }.exceptionOrNull() as TargetActionFailure

        assertEquals(TargetActionFailureKind.POLICY_DENIED, failure.kind)
        assertEquals(1, policyChecks)
        assertEquals(0, grants.validated.size)
        assertEquals(0, grants.consumeAttempts.size)
        assertEquals(0, beforeEvidence)
        assertEquals(0, dispatched)
    }

    @Test
    fun `two coordinators for one target share a nonqueueing writer`() {
        val input = stringCodec("schema_input0031", "app://fixture/shared-writer@1")
        val started = CountDownLatch(1)
        val release = CountDownLatch(1)
        val firstCalls = AtomicInteger()
        val secondCalls = AtomicInteger()
        val first = SemanticRegistryBuilder()
            .registerAction(
                "shared.writer.one",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            ) {
                firstCalls.incrementAndGet()
                started.countDown()
                release.await(5, TimeUnit.SECONDS)
            }
            .freeze(CatalogIdentity("catalog_process0031", 31), 1_024)
            .targetActionCoordinator(
                targetId = "target_shared_writer",
                policyResolver = ExactPolicyResolver,
                destructiveAuthorizationValidator = RefusingGrantValidator,
                evidence = NoopEvidence,
            )
        val second = SemanticRegistryBuilder()
            .registerAction(
                "shared.writer.two",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { secondCalls.incrementAndGet() }
            .freeze(CatalogIdentity("catalog_process0032", 32), 1_024)
            .targetActionCoordinator(
                targetId = "target_shared_writer",
                policyResolver = ExactPolicyResolver,
                destructiveAuthorizationValidator = RefusingGrantValidator,
                evidence = NoopEvidence,
            )
        val executor = Executors.newSingleThreadExecutor()
        try {
            val occupying = executor.submit<TargetActionResult> {
                first.invoke(
                    TargetActionRequest(
                        ActionInvocation("shared.writer.one", 1, input.schema.handle, JsonPrimitive("go")),
                        TargetActionContext("target_shared_writer", 31, "session_shared_one"),
                        null,
                        { true },
                    ),
                )
            }
            assertTrue(started.await(5, TimeUnit.SECONDS))
            val conflict = runCatching {
                second.invoke(
                    TargetActionRequest(
                        ActionInvocation("shared.writer.two", 1, input.schema.handle, JsonPrimitive("go")),
                        TargetActionContext("target_shared_writer", 32, "session_shared_two"),
                        null,
                        { true },
                    ),
                )
            }.exceptionOrNull() as TargetActionFailure
            assertEquals(TargetActionFailureKind.CONFLICT, conflict.kind)
            assertEquals(0, secondCalls.get())
            release.countDown()
            assertEquals(TargetActionResult.COMPLETED, occupying.get(5, TimeUnit.SECONDS))
            assertEquals(1, firstCalls.get())
        } finally {
            release.countDown()
            executor.shutdownNow()
        }
    }

    @Test
    fun `writer conflict happens after read-only grant validation and does not consume`() {
        val holdInput = stringCodec("schema_input0017", "app://fixture/hold@1")
        val grantInput = stringCodec("schema_input0018", "app://fixture/granted@1")
        val started = CountDownLatch(1)
        val release = CountDownLatch(1)
        val grantedCalls = AtomicInteger()
        val grants = TwoPhaseGrantStore("grant")
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "writer.hold",
                1,
                holdInput,
                ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            ) {
                started.countDown()
                release.await(5, TimeUnit.SECONDS)
            }
            .registerAction(
                "writer.granted",
                1,
                grantInput,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { grantedCalls.incrementAndGet() }
            .freeze(CatalogIdentity("catalog_process0017", 17), 1_024)
        val coordinator = registry.targetActionCoordinator(
            targetId = "target_conflict_test",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = grants,
            evidence = NoopEvidence,
        )
        val executor = Executors.newSingleThreadExecutor()
        try {
            val held = executor.submit<TargetActionResult> {
                coordinator.invoke(
                    TargetActionRequest(
                        ActionInvocation("writer.hold", 1, holdInput.schema.handle, JsonPrimitive("hold")),
                        TargetActionContext("target_conflict_test", 17, "session_conflict_test"),
                        null,
                        { true },
                    ),
                )
            }
            assertTrue(started.await(5, TimeUnit.SECONDS))
            val request = TargetActionRequest(
                ActionInvocation("writer.granted", 1, grantInput.schema.handle, JsonPrimitive("go")),
                TargetActionContext("target_conflict_test", 17, "session_conflict_test"),
                "grant",
                { true },
            )
            val conflict = runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure
            assertEquals(TargetActionFailureKind.CONFLICT, conflict.kind)
            assertEquals(1, grants.validated.size)
            assertEquals(0, grants.consumeAttempts.size)
            assertEquals(0, grantedCalls.get())

            release.countDown()
            assertEquals(TargetActionResult.COMPLETED, held.get(5, TimeUnit.SECONDS))
            assertEquals(TargetActionResult.COMPLETED, coordinator.invoke(request))
            assertEquals(1, grants.consumeAttempts.size)
            assertEquals(1, grantedCalls.get())
        } finally {
            release.countDown()
            executor.shutdownNow()
        }
    }

    @Test
    fun `changed binding rejects consume without dispatch`() {
        val input = stringCodec("schema_input0019", "app://fixture/binding-change@1")
        var dispatched = 0
        var expected: CanonicalActionBinding? = null
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "binding.changed",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { dispatched += 1 }
            .freeze(CatalogIdentity("catalog_process0019", 19), 1_024)
        val validator = object : DestructiveAuthorizationValidator {
            override fun validate(request: DestructiveAuthorizationRequest): Boolean {
                if (expected == null) expected = request.binding
                return request.binding == expected
            }

            override fun consume(request: DestructiveAuthorizationRequest): Boolean {
                expected = expected?.copy(inputDigest = "sha256:" + "0".repeat(64))
                return false
            }
        }
        val coordinator = registry.targetActionCoordinator(
            targetId = "target_binding_change",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = validator,
            evidence = NoopEvidence,
        )
        val request = TargetActionRequest(
            ActionInvocation("binding.changed", 1, input.schema.handle, JsonPrimitive("go")),
            TargetActionContext("target_binding_change", 19, "session_binding_change"),
            "grant",
            { true },
        )

        val failure = runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure
        assertEquals(TargetActionFailureKind.POLICY_DENIED, failure.kind)
        assertEquals(0, dispatched)
        assertEquals(
            TargetActionFailureKind.POLICY_DENIED,
            (runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure).kind,
        )
        assertEquals(0, dispatched)
    }

    @Test
    fun `consume throw leaves the grant unconsumed`() {
        val input = stringCodec("schema_input0025", "app://fixture/consume-throw@1")
        var dispatched = 0
        var throwOnConsume = true
        val consumed = AtomicBoolean(false)
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "consume.throws",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { dispatched += 1 }
            .freeze(CatalogIdentity("catalog_process0025", 25), 1_024)
        val validator = object : DestructiveAuthorizationValidator {
            override fun validate(request: DestructiveAuthorizationRequest) = !consumed.get()

            override fun consume(request: DestructiveAuthorizationRequest): Boolean {
                if (throwOnConsume) throw IllegalStateException("store failed")
                return consumed.compareAndSet(false, true)
            }
        }
        val coordinator = registry.targetActionCoordinator(
            targetId = "target_consume_throw",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = validator,
            evidence = NoopEvidence,
        )
        val request = TargetActionRequest(
            ActionInvocation("consume.throws", 1, input.schema.handle, JsonPrimitive("go")),
            TargetActionContext("target_consume_throw", 25, "session_consume_throw"),
            "grant",
            { true },
        )

        assertEquals(
            TargetActionFailureKind.PRE_DISPATCH_FAILED,
            (runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure).kind,
        )
        assertFalse(consumed.get())
        assertEquals(0, dispatched)

        throwOnConsume = false
        assertEquals(TargetActionResult.COMPLETED, coordinator.invoke(request))
        assertTrue(consumed.get())
        assertEquals(1, dispatched)
    }

    @Test
    fun `failure after consume but before dispatch is known and releases writer`() {
        val input = stringCodec("schema_input0020", "app://fixture/pre-dispatch@1")
        val calls = AtomicInteger()
        val grants = TwoPhaseGrantStore("grant")
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "pre.dispatch.failed",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { calls.incrementAndGet() }
            .freeze(CatalogIdentity("catalog_process0020", 20), 1_024)
        val coordinator = registry.targetActionMutationCoordinator(
            targetId = "target_pre_dispatch",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = grants,
            evidence = object : ActionEvidencePort {
                override fun captureBefore(context: TargetActionContext) {
                    throw IllegalStateException("no before evidence")
                }
                override fun observeStability(context: TargetActionContext) = Unit
                override fun captureAfter(context: TargetActionContext) = Unit
            },
        )
        val context = TargetActionContext("target_pre_dispatch", 20, "session_pre_dispatch")
        val request = TargetActionRequest(
            ActionInvocation("pre.dispatch.failed", 1, input.schema.handle, JsonPrimitive("go")),
            context,
            "grant",
            { true },
        )

        val failure = runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure
        assertEquals(TargetActionFailureKind.PRE_DISPATCH_FAILED, failure.kind)
        assertEquals(1, grants.consumeAttempts.size)
        assertEquals(0, calls.get())
        assertEquals(
            TargetActionFailureKind.POLICY_DENIED,
            (runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure).kind,
        )

        val releasedWriterCoordinator = registry.targetActionMutationCoordinator(
            targetId = "target_pre_dispatch",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = TwoPhaseGrantStore("unused"),
            evidence = NoopEvidence,
        )
        val ordinary = releasedWriterCoordinator.invokeOrdinary(
            OrdinaryMutationRequest(
                context,
                ActionPolicySubject("ordinary.after.failure", AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
                null,
                { true },
            ) { },
        )
        assertEquals(TargetActionResult.COMPLETED, ordinary)
        assertEquals(0, calls.get())
    }

    @Test
    fun `final session loss after consume remains known non-dispatch and releases writer`() {
        val input = stringCodec("schema_input0021", "app://fixture/final-session@1")
        val calls = AtomicInteger()
        val grants = TwoPhaseGrantStore("grant")
        var active = true
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "final.session",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { calls.incrementAndGet() }
            .freeze(CatalogIdentity("catalog_process0021", 21), 1_024)
        val coordinator = registry.targetActionCoordinator(
            targetId = "target_final_session",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = grants,
            evidence = object : ActionEvidencePort {
                override fun captureBefore(context: TargetActionContext) {
                    active = false
                }
                override fun observeStability(context: TargetActionContext) = Unit
                override fun captureAfter(context: TargetActionContext) = Unit
            },
        )
        val request = TargetActionRequest(
            ActionInvocation("final.session", 1, input.schema.handle, JsonPrimitive("go")),
            TargetActionContext("target_final_session", 21, "session_final_session"),
            "grant",
            { active },
        )

        val failure = runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure
        assertEquals(TargetActionFailureKind.SESSION_EXPIRED, failure.kind)
        assertEquals(1, grants.consumeAttempts.size)
        assertEquals(0, calls.get())

        active = true
        assertEquals(
            TargetActionFailureKind.POLICY_DENIED,
            (runCatching { coordinator.invoke(request) }.exceptionOrNull() as TargetActionFailure).kind,
        )
        val releasedWriterCoordinator = registry.targetActionMutationCoordinator(
            targetId = "target_final_session",
            policyResolver = ExactPolicyResolver,
            destructiveAuthorizationValidator = TwoPhaseGrantStore("unused"),
            evidence = NoopEvidence,
        )
        assertEquals(
            TargetActionResult.COMPLETED,
            releasedWriterCoordinator.invokeOrdinary(
                OrdinaryMutationRequest(
                    TargetActionContext("target_final_session", 21, "session_final_session"),
                    ActionPolicySubject(
                        "ordinary.final.session",
                        AuthorizationPolicy.NONE,
                        RetrySafety.NO_AUTOMATIC_RETRY,
                    ),
                    null,
                    { true },
                ) { },
            ),
        )
        assertEquals(0, calls.get())
    }

    @Test
    fun `post dispatch evidence failure is outcome unknown and writer is released`() {
        val calls = AtomicInteger()
        val input = stringCodec("schema_input0015", "app://fixture/evidence@1")
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "evidence.action",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { calls.incrementAndGet() }
            .freeze(CatalogIdentity("catalog_process0015", 15), 1_024)
        val coordinator = registry.targetActionCoordinator(
            targetId = "target_evidence_test",
            policyResolver = EffectiveActionPolicyResolver { _, declaration ->
                EffectiveActionPolicy(declaration.authorization, declaration.retrySafety)
            },
            destructiveAuthorizationValidator = RefusingGrantValidator,
            evidence = object : ActionEvidencePort {
                override fun captureBefore(context: TargetActionContext) = Unit
                override fun observeStability(context: TargetActionContext) = Unit
                override fun captureAfter(context: TargetActionContext) {
                    throw IllegalStateException("lost evidence")
                }
            },
        )
        val request = TargetActionRequest(
            ActionInvocation("evidence.action", 1, input.schema.handle, JsonPrimitive("go")),
            TargetActionContext("target_evidence_test", 15, "session_evidence_test"),
            null,
            { true },
        )

        assertEquals(
            TargetActionFailureKind.OUTCOME_UNKNOWN,
            runCatching { coordinator.invoke(request) }.exceptionOrNull()
                .let { it as? TargetActionFailure }?.kind,
        )
        assertEquals(1, calls.get())
        assertEquals(
            TargetActionFailureKind.OUTCOME_UNKNOWN,
            runCatching { coordinator.invoke(request) }.exceptionOrNull()
                .let { it as? TargetActionFailure }?.kind,
        )
        assertEquals(2, calls.get())
    }

    @Test
    fun `every post handoff failure is outcome unknown without automatic retry`() {
        for (stage in listOf("handler", "session", "stability", "after")) {
            val input = stringCodec("schema_input0022", "app://fixture/post-handoff@1")
            val calls = AtomicInteger()
            var active = true
            val registry = SemanticRegistryBuilder()
                .registerAction(
                    "post.handoff.$stage",
                    1,
                    input,
                    ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
                ) {
                    calls.incrementAndGet()
                    if (stage == "handler") throw IllegalStateException("handler failed")
                    if (stage == "session") active = false
                }
                .freeze(CatalogIdentity("catalog_posthandoff$stage", 22), 1_024)
            val coordinator = registry.targetActionCoordinator(
                targetId = "target_post_handoff",
                policyResolver = ExactPolicyResolver,
                destructiveAuthorizationValidator = RefusingGrantValidator,
                evidence = object : ActionEvidencePort {
                    override fun captureBefore(context: TargetActionContext) = Unit
                    override fun observeStability(context: TargetActionContext) {
                        if (stage == "stability") throw IllegalStateException("unstable")
                    }
                    override fun captureAfter(context: TargetActionContext) {
                        if (stage == "after") throw IllegalStateException("lost after evidence")
                    }
                },
            )
            val failure = runCatching {
                coordinator.invoke(
                    TargetActionRequest(
                        ActionInvocation("post.handoff.$stage", 1, input.schema.handle, JsonPrimitive("go")),
                        TargetActionContext("target_post_handoff", 22, "session_post_handoff"),
                        null,
                        { active },
                    ),
                )
            }.exceptionOrNull() as TargetActionFailure

            assertEquals(TargetActionFailureKind.OUTCOME_UNKNOWN, failure.kind, stage)
            assertEquals(1, calls.get(), stage)
        }
    }

    @Test
    fun `semantic and ordinary mutations share one nonqueueing writer`() {
        val semanticStarted = CountDownLatch(1)
        val semanticRelease = CountDownLatch(1)
        val ordinaryStarted = CountDownLatch(1)
        val ordinaryRelease = CountDownLatch(1)
        val input = stringCodec("schema_input0014", "app://fixture/writer@1")
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "writer.semantic",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            ) {
                semanticStarted.countDown()
                semanticRelease.await(5, TimeUnit.SECONDS)
            }
            .freeze(CatalogIdentity("catalog_process0014", 14), 1_024)
        val coordinator = registry.targetActionMutationCoordinator(
            targetId = "target_writer_test",
            policyResolver = EffectiveActionPolicyResolver { _, declaration ->
                EffectiveActionPolicy(declaration.authorization, declaration.retrySafety)
            },
            destructiveAuthorizationValidator = RefusingGrantValidator,
            evidence = object : ActionEvidencePort {
                override fun captureBefore(context: TargetActionContext) = Unit
                override fun observeStability(context: TargetActionContext) = Unit
                override fun captureAfter(context: TargetActionContext) = Unit
            },
        )
        val context = TargetActionContext("target_writer_test", 14, "session_writer_test")
        val semantic = TargetActionRequest(
            ActionInvocation("writer.semantic", 1, input.schema.handle, JsonPrimitive("go")),
            context,
            null,
            { true },
        )
        val ordinary = OrdinaryMutationRequest(
            context,
            ActionPolicySubject(
                "ordinary.writer",
                AuthorizationPolicy.NONE,
                RetrySafety.NO_AUTOMATIC_RETRY,
            ),
            null,
            { true },
        ) {
            ordinaryStarted.countDown()
            ordinaryRelease.await(5, TimeUnit.SECONDS)
        }
        val executor = Executors.newFixedThreadPool(2)
        try {
            val firstOrdinary = executor.submit<TargetActionResult> { coordinator.invokeOrdinary(ordinary) }
            assertTrue(ordinaryStarted.await(5, TimeUnit.SECONDS))
            assertEquals(
                TargetActionFailureKind.CONFLICT,
                runCatching { coordinator.invoke(semantic) }.exceptionOrNull()
                    .let { it as? TargetActionFailure }?.kind,
            )
            ordinaryRelease.countDown()
            assertEquals(TargetActionResult.COMPLETED, firstOrdinary.get(5, TimeUnit.SECONDS))

            val firstSemantic = executor.submit<TargetActionResult> { coordinator.invoke(semantic) }
            assertTrue(semanticStarted.await(5, TimeUnit.SECONDS))
            assertEquals(
                TargetActionFailureKind.CONFLICT,
                runCatching { coordinator.invokeOrdinary(ordinary) }.exceptionOrNull()
                    .let { it as? TargetActionFailure }?.kind,
            )
            semanticRelease.countDown()
            assertEquals(TargetActionResult.COMPLETED, firstSemantic.get(5, TimeUnit.SECONDS))
        } finally {
            ordinaryRelease.countDown()
            semanticRelease.countDown()
            executor.shutdownNow()
        }
    }

    @Test
    fun `different targets run in parallel and share an atomic grant store`() {
        val input = stringCodec("schema_input0023", "app://fixture/targets@1")
        val bothStarted = CountDownLatch(2)
        val release = CountDownLatch(1)
        val calls = AtomicInteger()
        val registry = SemanticRegistryBuilder()
            .registerAction(
                "targets.parallel",
                1,
                input,
                ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            ) {
                calls.incrementAndGet()
                bothStarted.countDown()
                release.await(5, TimeUnit.SECONDS)
            }
            .freeze(CatalogIdentity("catalog_process0023", 23), 1_024)
        val coordinatorFor: (String) -> TargetActionCoordinator = { targetId ->
            registry.targetActionCoordinator(
                targetId = targetId,
                policyResolver = ExactPolicyResolver,
                destructiveAuthorizationValidator = RefusingGrantValidator,
                evidence = NoopEvidence,
            )
        }
        val first = coordinatorFor("target_first")
        val second = coordinatorFor("target_second")
        val requestFor: (String) -> TargetActionRequest = { targetId ->
            TargetActionRequest(
                ActionInvocation("targets.parallel", 1, input.schema.handle, JsonPrimitive("go")),
                TargetActionContext(targetId, 23, "session_$targetId"),
                null,
                { true },
            )
        }
        val executor = Executors.newFixedThreadPool(2)
        try {
            val firstResult = executor.submit<TargetActionResult> { first.invoke(requestFor("target_first")) }
            val secondResult = executor.submit<TargetActionResult> { second.invoke(requestFor("target_second")) }
            assertTrue(bothStarted.await(5, TimeUnit.SECONDS))
            release.countDown()
            assertEquals(TargetActionResult.COMPLETED, firstResult.get(5, TimeUnit.SECONDS))
            assertEquals(TargetActionResult.COMPLETED, secondResult.get(5, TimeUnit.SECONDS))
            assertEquals(2, calls.get())
        } finally {
            release.countDown()
            executor.shutdownNow()
        }

        val consumed = AtomicBoolean(false)
        val sharedGrants = object : DestructiveAuthorizationValidator {
            override fun validate(request: DestructiveAuthorizationRequest) = !consumed.get()
            override fun consume(request: DestructiveAuthorizationRequest) = consumed.compareAndSet(false, true)
        }
        val destructiveInput = stringCodec("schema_input0024", "app://fixture/shared-grant@1")
        val destructiveRegistry = SemanticRegistryBuilder()
            .registerAction(
                "targets.shared_grant",
                1,
                destructiveInput,
                ActionPolicy(AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION, RetrySafety.NO_AUTOMATIC_RETRY),
            ) { }
            .freeze(CatalogIdentity("catalog_process0024", 24), 1_024)
        val destructiveRequestFor: (String) -> TargetActionRequest = { targetId ->
            TargetActionRequest(
                ActionInvocation("targets.shared_grant", 1, destructiveInput.schema.handle, JsonPrimitive("go")),
                TargetActionContext(targetId, 24, "session_shared_$targetId"),
                "grant",
                { true },
            )
        }
        val firstDestructive = destructiveRegistry.targetActionCoordinator(
            "target_shared_first",
            ExactPolicyResolver,
            sharedGrants,
            NoopEvidence,
        )
        val secondDestructive = destructiveRegistry.targetActionCoordinator(
            "target_shared_second",
            ExactPolicyResolver,
            sharedGrants,
            NoopEvidence,
        )
        val outcomes = listOf(
            runCatching { firstDestructive.invoke(destructiveRequestFor("target_shared_first")) }.exceptionOrNull(),
            runCatching { secondDestructive.invoke(destructiveRequestFor("target_shared_second")) }.exceptionOrNull(),
        )
        assertEquals(1, outcomes.count { it == null })
        assertEquals(
            TargetActionFailureKind.POLICY_DENIED,
            (outcomes.first { it != null } as TargetActionFailure).kind,
        )
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
        assertEquals(Json.parseToJsonElement("1000000000000000000000"), large.value)
        assertEquals("1000000000000000000000".toByteArray().size, large.bytes)
        val wideInteger = queryNumber("number.wide_integer", "999999999999999999")
        assertEquals(Json.parseToJsonElement("999999999999999999"), wideInteger.value)
        assertEquals("999999999999999999".toByteArray().size, wideInteger.bytes)
    }

    @Test
    fun `action bindings follow shared protocol canonical cases`() {
        val bindingCases = listOf(
            File("../protocol/v1.2/action-binding-cases.json"),
            File("../../protocol/v1.2/action-binding-cases.json"),
        ).firstOrNull(File::exists) ?: fail("action-binding-cases.json was not found")
        val fixture = Json.parseToJsonElement(
            bindingCases.readText(),
        ).jsonObject
        assertEquals("sha256", fixture["algorithm"]!!.jsonPrimitive.content)
        assertEquals("sha256:", fixture["digestPrefix"]!!.jsonPrimitive.content)
        val cases = fixture["cases"]!!.jsonArray.map { it.jsonObject }

        assertTrue(cases.map { it["id"]!!.jsonPrimitive.content }.containsAll(listOf(
            "object-key-order",
            "unicode-key-order",
            "escape-equivalent",
            "unicode-no-normalization",
            "int64-exact",
            "negative-zero",
            "finite-decimal",
            "finite-exponent-tiny",
            "finite-exponent-huge",
            "nan-inf-fail-closed",
        )))

        for (case in cases) {
            val id = case["id"]!!.jsonPrimitive.content
            val expect = case["expect"]!!.jsonPrimitive.content
            if (expect == "failClosed") {
                val synthetic = case["synthetic"]!!.jsonArray.map { it.jsonPrimitive.content }
                for (token in synthetic) {
                    val value = when (token) {
                        "NaN" -> JsonPrimitive(Double.NaN)
                        "Infinity" -> JsonPrimitive(Double.POSITIVE_INFINITY)
                        "-Infinity" -> JsonPrimitive(Double.NEGATIVE_INFINITY)
                        else -> fail("Unknown synthetic number $token")
                    }
                    val failure = try {
                        actionInputDigest(value)
                        fail("$id/$token should fail closed")
                    } catch (failure: SemanticFailure) {
                        failure
                    }
                    assertTrue(
                        failure.kind == SemanticFailureKind.SCHEMA_MISMATCH ||
                            failure.kind == SemanticFailureKind.DISCLOSURE_DENIED,
                    )
                }
                continue
            }

            val digests = case["jsonTexts"]!!.jsonArray.map { text ->
                actionInputDigest(Json.parseToJsonElement(text.jsonPrimitive.content))
            }
            if (expect == "sameDigest") {
                assertEquals(1, digests.toSet().size, id)
                case["digest"]?.jsonPrimitive?.content?.let { expected ->
                    assertEquals(expected, digests[0], id)
                }
            } else {
                assertEquals("distinctDigest", expect, id)
                assertEquals(digests.size, digests.toSet().size, id)
                case["digests"]?.jsonArray?.map { it.jsonPrimitive.content }?.let { expected ->
                    assertEquals(expected, digests, id)
                }
            }
        }
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

private fun actionInputDigest(input: JsonElement): String {
    val inputSchema = schemaForValue(input)
    val grants = TwoPhaseGrantStore("grant")
    val registry = SemanticRegistryBuilder()
        .registerAction(
            id = "binding.case",
            declarationRevision = 1,
            inputCodec = JsonElementCodec(inputSchema),
            policy = ActionPolicy(
                AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION,
                RetrySafety.NO_AUTOMATIC_RETRY,
            ),
        ) { }
        .freeze(CatalogIdentity("catalog_bindingcase0001", 1), 4_096)
    val coordinator = registry.targetActionCoordinator(
        targetId = "target_binding_test",
        policyResolver = EffectiveActionPolicyResolver { _, subject ->
            EffectiveActionPolicy(subject.authorization, subject.retrySafety)
        },
        destructiveAuthorizationValidator = grants,
        evidence = NoopEvidence,
    )
    val result = coordinator.invoke(
        TargetActionRequest(
            ActionInvocation("binding.case", 1, inputSchema.handle, input),
            TargetActionContext("target_binding_test", 1, "session_binding_test"),
            "grant",
            { true },
        ),
    )
    assertEquals(TargetActionResult.COMPLETED, result)
    val request = grants.validated.single()
    assertEquals("target_binding_test", request.binding.targetId)
    assertEquals(1, request.binding.processGeneration)
    assertEquals("session_binding_test", request.binding.sessionId)
    assertEquals("binding.case", request.binding.capability)
    assertEquals(1, request.binding.declarationRevision)
    assertEquals(inputSchema.handle, request.binding.inputSchema)
    assertTrue(request.binding.inputDigest.startsWith("sha256:"))
    return request.binding.inputDigest
}

private fun schemaForValue(value: JsonElement): SemanticSchema {
    val type = when (value) {
        JsonNull -> "null"
        is JsonArray -> "array"
        is JsonObject -> "object"
        is JsonPrimitive -> when {
            value.isString -> "string"
            value.booleanOrNull != null -> "boolean"
            else -> "number"
        }
    }
    return SemanticSchema.create(
        "schema_binding_input",
        1,
        schemaDocumentForValue(type, value),
    )
}

private fun schemaDocumentForValue(type: String, value: JsonElement): JsonObject = buildJsonObject {
    put("\$schema", "https://json-schema.org/draft/2020-12/schema")
    put("\$id", "app://fixture/binding-input@1")
    put("type", type)
    when (value) {
        is JsonObject -> {
            putJsonObject("properties") {
                value.forEach { (name, child) ->
                    putJsonObject(name) { put("type", schemaDocumentForValue(jsonType(child), child)["type"]!!.jsonPrimitive.content) }
                }
            }
            put("additionalProperties", false)
        }
        is JsonArray -> putJsonObject("items") {
            put("type", value.firstOrNull()?.let(::jsonType) ?: "null")
        }
        else -> Unit
    }
}

private fun jsonType(value: JsonElement): String = when (value) {
    JsonNull -> "null"
    is JsonArray -> "array"
    is JsonObject -> "object"
    is JsonPrimitive -> when {
        value.isString -> "string"
        value.booleanOrNull != null -> "boolean"
        else -> "number"
    }
}

private object NoopEvidence : ActionEvidencePort {
    override fun captureBefore(context: TargetActionContext) = Unit
    override fun observeStability(context: TargetActionContext) = Unit
    override fun captureAfter(context: TargetActionContext) = Unit
}

private object ExactPolicyResolver : EffectiveActionPolicyResolver {
    override fun resolve(context: TargetActionContext, subject: ActionPolicySubject) =
        EffectiveActionPolicy(subject.authorization, subject.retrySafety)
}

private object RefusingGrantValidator : DestructiveAuthorizationValidator {
    override fun validate(request: DestructiveAuthorizationRequest) = false
    override fun consume(request: DestructiveAuthorizationRequest) = false
}

private class TwoPhaseGrantStore(
    private val expectedGrant: String,
    private val consumed: AtomicBoolean = AtomicBoolean(false),
) : DestructiveAuthorizationValidator {
    val validated = mutableListOf<DestructiveAuthorizationRequest>()
    val consumeAttempts = mutableListOf<DestructiveAuthorizationRequest>()

    override fun validate(request: DestructiveAuthorizationRequest): Boolean {
        validated += request
        return request.grant == expectedGrant && !consumed.get()
    }

    override fun consume(request: DestructiveAuthorizationRequest): Boolean {
        consumeAttempts += request
        return request.grant == expectedGrant && consumed.compareAndSet(false, true)
    }
}

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

private class JsonElementCodec(
    override val schema: SemanticSchema,
) : SemanticCodec<JsonElement> {
    override fun decode(value: JsonElement): JsonElement = value
    override fun encode(value: JsonElement): EncodedSemanticValue = safePayload(
        schema,
        Json.encodeToString(JsonElement.serializer(), value),
    )
    override fun validates(value: JsonElement) = true
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
