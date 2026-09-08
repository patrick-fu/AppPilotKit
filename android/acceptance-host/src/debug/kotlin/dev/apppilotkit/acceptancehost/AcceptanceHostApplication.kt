package dev.apppilotkit.acceptancehost

import android.app.Application
import dev.apppilotkit.semantic.CatalogIdentity
import dev.apppilotkit.semantic.ClassificationStatus
import dev.apppilotkit.semantic.ActionEvidencePort
import dev.apppilotkit.semantic.ActionPolicy
import dev.apppilotkit.semantic.ActionInvocation
import dev.apppilotkit.semantic.AuthorizationPolicy
import dev.apppilotkit.semantic.DestructiveAuthorizationRequest
import dev.apppilotkit.semantic.DestructiveAuthorizationValidator
import dev.apppilotkit.semantic.EncodedSemanticValue
import dev.apppilotkit.semantic.EffectiveActionPolicy
import dev.apppilotkit.semantic.EffectiveActionPolicyResolver
import dev.apppilotkit.semantic.RedactionStatus
import dev.apppilotkit.semantic.RetrySafety
import dev.apppilotkit.semantic.SemanticCodec
import dev.apppilotkit.semantic.SemanticRegistryBuilder
import dev.apppilotkit.semantic.SemanticSchema
import dev.apppilotkit.semantic.TargetActionCoordinator
import dev.apppilotkit.semantic.TargetActionContext
import dev.apppilotkit.semantic.TargetActionFailure
import dev.apppilotkit.semantic.TargetActionFailureKind
import dev.apppilotkit.semantic.TargetActionRequest
import dev.apppilotkit.semantic.TargetActionResult
import dev.apppilotkit.semantic.runtime.ProtocolRuntimeLimits
import dev.apppilotkit.semantic.runtime.SemanticProtocolPolicy
import dev.apppilotkit.targettransport.internal.TargetRuntimeComposition
import dev.apppilotkit.targettransport.internal.TargetTransportBootstrap
import java.nio.charset.StandardCharsets
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put

class AcceptanceHostApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        TargetTransportBootstrap.install(::createCatalogComposition)
    }
}

internal fun createFoundationComposition(processGeneration: Long): TargetRuntimeComposition {
    val catalog = SemanticRegistryBuilder()
        .registerResource(FOUNDATION_RESOURCE_ID, 1, FoundationStateCodec) { FOUNDATION_STATE }
        .freeze(CatalogIdentity("catalog_acceptancehost01", processGeneration), MAX_DISCLOSURE_BYTES)

    return TargetRuntimeComposition(
        catalog = catalog,
        limits = ProtocolRuntimeLimits(
            maxRequestBytes = MAX_REQUEST_BYTES,
            maxResponseBytes = MAX_RESPONSE_BYTES,
            maxPageItems = MAX_PAGE_ITEMS,
        ),
        policy = SemanticProtocolPolicy(
            discover = { _, declaration -> declaration.id == FOUNDATION_RESOURCE_ID },
            discloseSchema = { _, declaration -> declaration.id == FOUNDATION_RESOURCE_ID },
            discloseResource = { _, resource -> resource.id == FOUNDATION_RESOURCE_ID },
            discloseAction = { _, _ -> false },
        ),
        actionCoordinator = NoActionCoordinator,
        targetId = "target_acceptancehost",
    )
}

private object FoundationStateCodec : SemanticCodec<JsonObject> {
    override val schema: SemanticSchema = SemanticSchema.create(
        id = "schema_demo_foundation_state_v1",
        revision = 1,
        document = buildJsonObject {
            put("\$schema", "https://json-schema.org/draft/2020-12/schema")
            put("\$id", "app://acceptance.foundation.state/value@1")
            put("type", "object")
            put("required", JsonArray(listOf(
                JsonPrimitive("scenario"),
                JsonPrimitive("seed"),
                JsonPrimitive("platform"),
            )))
            put("properties", buildJsonObject {
                put("scenario", buildJsonObject {
                    put("type", "string")
                    put("const", "demo.foundation")
                })
                put("seed", buildJsonObject {
                    put("type", "string")
                    put("const", "foundation-v1")
                })
                put("platform", buildJsonObject {
                    put("type", "string")
                })
            })
            put("additionalProperties", false)
        },
    )

    override fun decode(value: JsonElement): JsonObject = value.jsonObject

    override fun encode(value: JsonObject): EncodedSemanticValue = EncodedSemanticValue(
        utf8 = Json.encodeToString(JsonObject.serializer(), value).toByteArray(StandardCharsets.UTF_8),
        schema = schema.handle,
        classification = ClassificationStatus.COMPLETE,
        redaction = RedactionStatus.COMPLETE,
    )

    override fun validates(value: JsonElement): Boolean = value == FOUNDATION_STATE
}

private object NoActionCoordinator : TargetActionCoordinator {
    override fun invoke(request: TargetActionRequest): TargetActionResult =
        error("Acceptance Host registers no actions")
}

private val FOUNDATION_STATE = buildJsonObject {
    put("scenario", "demo.foundation")
    put("seed", "foundation-v1")
    put("platform", "android")
}

private const val FOUNDATION_RESOURCE_ID = "acceptance.foundation.state"
private const val MAX_DISCLOSURE_BYTES = 4 * 1024
private const val MAX_REQUEST_BYTES = 4 * 1024
private const val MAX_RESPONSE_BYTES = 16 * 1024
private const val MAX_PAGE_ITEMS = 16

/** The resettable public Demo Scenario used by the installed Journey 7 harness. */
internal fun createCatalogComposition(processGeneration: Long): TargetRuntimeComposition {
    val state = CatalogState()
    val catalog = SemanticRegistryBuilder()
        .registerResource(
            id = CATALOG_RESOURCE_ID,
            declarationRevision = 1,
            valueCodec = CatalogStateCodec,
            available = { state.count() != 1L },
        ) { state.snapshot() }
        .registerAction(
            id = CATALOG_INCREMENT_ACTION_ID,
            declarationRevision = 1,
            inputCodec = CatalogIncrementCodec,
            policy = ActionPolicy(AuthorizationPolicy.NONE, RetrySafety.NO_AUTOMATIC_RETRY),
            available = { true },
        ) { input -> state.increment(input.amount) }
        .registerAction(
            id = CATALOG_RESET_ACTION_ID,
            declarationRevision = 1,
            inputCodec = CatalogResetCodec,
            policy = ActionPolicy(
                AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION,
                RetrySafety.RETRY_WITH_PROOF_ONLY,
            ),
            available = { true },
        ) { state.reset() }
        .freeze(CatalogIdentity(CATALOG_ID, processGeneration), MAX_DISCLOSURE_BYTES)

    val actionCoordinator = catalog.targetActionCoordinator(
        targetId = CATALOG_TARGET_ID,
        policyResolver = EffectiveActionPolicyResolver { _, subject ->
            EffectiveActionPolicy(subject.authorization, subject.retrySafety)
        },
        destructiveAuthorizationValidator = RefusingCatalogGrantValidator,
        evidence = CatalogActionEvidence,
    )

    return TargetRuntimeComposition(
        catalog = catalog,
        limits = ProtocolRuntimeLimits(
            maxRequestBytes = MAX_REQUEST_BYTES,
            maxResponseBytes = MAX_RESPONSE_BYTES,
            maxPageItems = MAX_PAGE_ITEMS,
        ),
        policy = SemanticProtocolPolicy(
            discover = { _, declaration -> declaration.id in CATALOG_CAPABILITY_IDS },
            discloseSchema = { _, declaration -> declaration.id in CATALOG_CAPABILITY_IDS },
            discloseResource = { _, resource -> resource.id == CATALOG_RESOURCE_ID },
            discloseAction = { _, declaration -> declaration.id in CATALOG_ACTION_IDS },
        ),
        actionCoordinator = GenerationBoundCoordinator(processGeneration, actionCoordinator),
        targetId = CATALOG_TARGET_ID,
    )
}

private class CatalogState {
    private val lock = Any()
    private var value = 0L

    fun count(): Long = synchronized(lock) { value }

    fun snapshot(): JsonObject = synchronized(lock) {
        buildJsonObject {
            put("scenario", CATALOG_SCENARIO)
            put("seed", CATALOG_SEED)
            put("platform", "android")
            put("count", value)
            put("available", value % 2L == 0L)
        }
    }

    fun increment(amount: Long) = synchronized(lock) {
        value += amount
    }

    fun reset() = synchronized(lock) {
        value = 0L
    }
}

private data class CatalogIncrementInput(val amount: Long)
private data class CatalogResetInput(val confirm: String)

private object CatalogStateCodec : SemanticCodec<JsonObject> {
    override val schema: SemanticSchema = CATALOG_STATE_SCHEMA

    override fun decode(value: JsonElement): JsonObject = value.jsonObject

    override fun encode(value: JsonObject): EncodedSemanticValue {
        val count = value["count"]?.jsonPrimitive?.longOrNull ?: -1L
        val output = when (count) {
            3L -> buildJsonObject {
                value.forEach { (key, child) -> put(key, child) }
                put("sensitive_canary", catalogSecretCanary())
                put("unclassified_canary", "unclassified")
            }
            4L -> buildJsonObject {
                value.forEach { (key, child) -> put(key, child) }
                put("platform", "x".repeat(5_000))
            }
            else -> value
        }
        val classification = if (count == 3L) ClassificationStatus.UNCLASSIFIED else ClassificationStatus.COMPLETE
        return EncodedSemanticValue(
            utf8 = Json.encodeToString(JsonObject.serializer(), output).toByteArray(StandardCharsets.UTF_8),
            schema = schema.handle,
            classification = classification,
            redaction = RedactionStatus.COMPLETE,
        )
    }

    override fun validates(value: JsonElement): Boolean {
        val objectValue = value as? JsonObject ?: return false
        val scenario = objectValue["scenario"]?.jsonPrimitive?.contentOrNull
        val seed = objectValue["seed"]?.jsonPrimitive?.contentOrNull
        val platform = objectValue["platform"]?.jsonPrimitive?.contentOrNull
        val count = objectValue["count"]?.jsonPrimitive?.longOrNull
        val available = objectValue["available"]?.jsonPrimitive?.booleanOrNull
        return scenario == CATALOG_SCENARIO && seed == CATALOG_SEED &&
            (platform == "android" || (count == 4L && platform != null)) &&
            count != null && count >= 0L &&
            available == (count % 2L == 0L)
    }
}

private object CatalogIncrementCodec : SemanticCodec<CatalogIncrementInput> {
    override val schema: SemanticSchema = CATALOG_INCREMENT_SCHEMA

    override fun decode(value: JsonElement): CatalogIncrementInput = CatalogIncrementInput(
        value.jsonObject["amount"]?.jsonPrimitive?.longOrNull ?: error("amount"),
    )

    override fun encode(value: CatalogIncrementInput): EncodedSemanticValue = EncodedSemanticValue(
        utf8 = Json.encodeToString(JsonObject.serializer(), buildJsonObject { put("amount", value.amount) })
            .toByteArray(StandardCharsets.UTF_8),
        schema = schema.handle,
        classification = ClassificationStatus.COMPLETE,
        redaction = RedactionStatus.COMPLETE,
    )

    override fun validates(value: JsonElement): Boolean =
        value.jsonObject["amount"]?.jsonPrimitive?.longOrNull == 1L
}

private object CatalogResetCodec : SemanticCodec<CatalogResetInput> {
    override val schema: SemanticSchema = CATALOG_RESET_SCHEMA

    override fun decode(value: JsonElement): CatalogResetInput = CatalogResetInput(
        value.jsonObject["confirm"]?.jsonPrimitive?.contentOrNull ?: error("confirm"),
    )

    override fun encode(value: CatalogResetInput): EncodedSemanticValue = EncodedSemanticValue(
        utf8 = Json.encodeToString(JsonObject.serializer(), buildJsonObject { put("confirm", value.confirm) })
            .toByteArray(StandardCharsets.UTF_8),
        schema = schema.handle,
        classification = ClassificationStatus.COMPLETE,
        redaction = RedactionStatus.COMPLETE,
    )

    override fun validates(value: JsonElement): Boolean =
        value.jsonObject["confirm"]?.jsonPrimitive?.contentOrNull == CATALOG_RESET_CONFIRM
}

private object CatalogActionEvidence : ActionEvidencePort {
    private var before = 0
    private var stable = 0
    private var after = 0
    private val lock = Any()

    override fun captureBefore(context: TargetActionContext) = synchronized(lock) { before += 1 }
    override fun observeStability(context: TargetActionContext) = synchronized(lock) { stable += 1 }
    override fun captureAfter(context: TargetActionContext) = synchronized(lock) { after += 1 }
}

private object RefusingCatalogGrantValidator : DestructiveAuthorizationValidator {
    override fun validate(request: DestructiveAuthorizationRequest): Boolean = false
    override fun consume(request: DestructiveAuthorizationRequest): Boolean = false
}

private class GenerationBoundCoordinator(
    private val generation: Long,
    private val delegate: TargetActionCoordinator,
) : TargetActionCoordinator {
    override fun invoke(request: TargetActionRequest): TargetActionResult {
        if (request.context.processGeneration != generation) {
            throw TargetActionFailure(TargetActionFailureKind.SESSION_EXPIRED)
        }
        return delegate.invoke(request)
    }
}

private fun catalogSchemaDocument(
    id: String,
    required: JsonArray,
    properties: JsonObject,
    uri: String,
) = buildJsonObject {
    put("\$schema", "https://json-schema.org/draft/2020-12/schema")
    put("\$id", uri)
    put("type", "object")
    put("required", required)
    put("properties", properties)
    put("additionalProperties", false)
}

private val CATALOG_STATE_SCHEMA = SemanticSchema.create(
    id = "schema_demo_catalog_state_v1",
    revision = 1,
    document = catalogSchemaDocument(
        id = "schema_demo_catalog_state_v1",
        uri = "app://acceptance.catalog.state/value@1",
        required = JsonArray(listOf("scenario", "seed", "platform", "count", "available").map(::JsonPrimitive)),
        properties = buildJsonObject {
            put("scenario", buildJsonObject { put("type", "string"); put("const", CATALOG_SCENARIO) })
            put("seed", buildJsonObject { put("type", "string"); put("const", CATALOG_SEED) })
            put("platform", buildJsonObject { put("type", "string") })
            put("count", buildJsonObject { put("type", "integer"); put("minimum", 0) })
            put("available", buildJsonObject { put("type", "boolean") })
        },
    ),
)

private val CATALOG_INCREMENT_SCHEMA = SemanticSchema.create(
    id = "schema_demo_catalog_increment_input_v1",
    revision = 1,
    document = catalogSchemaDocument(
        id = "schema_demo_catalog_increment_input_v1",
        uri = "app://acceptance.catalog.increment/input@1",
        required = JsonArray(listOf(JsonPrimitive("amount"))),
        properties = buildJsonObject {
            put("amount", buildJsonObject { put("type", "integer"); put("const", 1) })
        },
    ),
)

private val CATALOG_RESET_SCHEMA = SemanticSchema.create(
    id = "schema_demo_catalog_reset_input_v1",
    revision = 1,
    document = catalogSchemaDocument(
        id = "schema_demo_catalog_reset_input_v1",
        uri = "app://acceptance.catalog.reset/input@1",
        required = JsonArray(listOf(JsonPrimitive("confirm"))),
        properties = buildJsonObject {
            put("confirm", buildJsonObject { put("type", "string"); put("const", CATALOG_RESET_CONFIRM) })
        },
    ),
)

private const val CATALOG_ID = "catalog_demo_catalog"
private const val CATALOG_TARGET_ID = "target_acceptance_catalog"
private const val CATALOG_SCENARIO = "demo.catalog"
private const val CATALOG_SEED = "catalog-v1"
private const val CATALOG_RESOURCE_ID = "acceptance.catalog.state"
private const val CATALOG_INCREMENT_ACTION_ID = "acceptance.catalog.increment"
private const val CATALOG_RESET_ACTION_ID = "acceptance.catalog.reset"
private const val CATALOG_RESET_CONFIRM = "reset-catalog-v1"
private fun catalogSecretCanary(): String = listOf(
    "APPPILOTKIT_DEMO_CATALOG_",
    "SECRET_CANARY_7f9c4b2e",
).joinToString(separator = "")
private val CATALOG_CAPABILITY_IDS = setOf(CATALOG_RESOURCE_ID, CATALOG_INCREMENT_ACTION_ID, CATALOG_RESET_ACTION_ID)
private val CATALOG_ACTION_IDS = setOf(CATALOG_INCREMENT_ACTION_ID, CATALOG_RESET_ACTION_ID)
