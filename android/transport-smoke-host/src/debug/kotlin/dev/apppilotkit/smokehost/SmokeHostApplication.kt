package dev.apppilotkit.smokehost

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
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put

class SmokeHostApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        TargetTransportBootstrap.install(::createComposition)
    }
}

private fun createComposition(processGeneration: Long): TargetRuntimeComposition {
    val catalog = SemanticRegistryBuilder()
        .registerResource("smoke.ready", 1, SmokeReadyCodec) {
            buildJsonObject { put("ready", true) }
        }
        .freeze(CatalogIdentity("catalog_smokehost0001", processGeneration), MAX_DISCLOSURE_BYTES)

    return TargetRuntimeComposition(
        catalog = catalog,
        limits = ProtocolRuntimeLimits(
            maxRequestBytes = MAX_REQUEST_BYTES,
            maxResponseBytes = MAX_RESPONSE_BYTES,
            maxPageItems = MAX_PAGE_ITEMS,
        ),
        policy = SemanticProtocolPolicy(
            discover = { _, declaration -> declaration.id == "smoke.ready" },
            discloseSchema = { _, declaration -> declaration.id == "smoke.ready" },
            discloseResource = { _, resource -> resource.id == "smoke.ready" },
            discloseAction = { _, _ -> false },
        ),
        actionCoordinator = NoActionCoordinator,
        targetId = "target_smokehost",
    )
}

private object SmokeReadyCodec : SemanticCodec<JsonObject> {
    override val schema: SemanticSchema = SemanticSchema.create(
        id = "schema_smoke_ready_v1",
        revision = 1,
        document = buildJsonObject {
            put("\$schema", "https://json-schema.org/draft/2020-12/schema")
            put("\$id", "app://smoke/ready@1")
            put("type", "object")
            put("properties", buildJsonObject {
                put("ready", buildJsonObject {
                    put("type", "boolean")
                    put("const", true)
                })
            })
            put("required", kotlinx.serialization.json.JsonArray(listOf(kotlinx.serialization.json.JsonPrimitive("ready"))))
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

    override fun validates(value: JsonElement): Boolean = value == READY_VALUE
}

private object NoActionCoordinator : TargetActionCoordinator {
    override fun invoke(request: TargetActionRequest): TargetActionResult =
        error("Smoke host registers no actions")
}

private val READY_VALUE = buildJsonObject { put("ready", true) }
private const val MAX_DISCLOSURE_BYTES = 4 * 1024
private const val MAX_REQUEST_BYTES = 4 * 1024
private const val MAX_RESPONSE_BYTES = 16 * 1024
private const val MAX_PAGE_ITEMS = 16
