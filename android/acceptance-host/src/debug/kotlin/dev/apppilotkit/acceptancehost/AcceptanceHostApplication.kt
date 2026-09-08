package dev.apppilotkit.acceptancehost

import android.app.Application
import dev.apppilotkit.semantic.CatalogIdentity
import dev.apppilotkit.semantic.ClassificationStatus
import dev.apppilotkit.semantic.EncodedSemanticValue
import dev.apppilotkit.semantic.RedactionStatus
import dev.apppilotkit.semantic.SemanticCodec
import dev.apppilotkit.semantic.SemanticRegistryBuilder
import dev.apppilotkit.semantic.SemanticSchema
import dev.apppilotkit.semantic.TargetActionCoordinator
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
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put

class AcceptanceHostApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        TargetTransportBootstrap.install(::createFoundationComposition)
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
