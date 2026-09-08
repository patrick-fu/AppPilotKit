package dev.apppilotkit.acceptancehost

import dev.apppilotkit.semantic.ActionDeclaration
import dev.apppilotkit.semantic.ActionInvocation
import dev.apppilotkit.semantic.CatalogIdentity
import dev.apppilotkit.semantic.ResourceDeclaration
import dev.apppilotkit.semantic.ResourceQuery
import dev.apppilotkit.semantic.SemanticFailure
import dev.apppilotkit.semantic.SemanticFailureKind
import dev.apppilotkit.semantic.TargetActionContext
import dev.apppilotkit.semantic.TargetActionFailure
import dev.apppilotkit.semantic.TargetActionFailureKind
import dev.apppilotkit.semantic.TargetActionRequest
import dev.apppilotkit.semantic.TargetActionResult
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put
import org.junit.Test

class CatalogCompositionTest {
    @Test
    fun catalog_membership_and_schemas_are_contract_stable() {
        val composition = createCatalogComposition(processGeneration = 7)
        assertEquals(
            listOf("acceptance.catalog.increment", "acceptance.catalog.reset", "acceptance.catalog.state"),
            composition.catalog.list().map { it.id },
        )
        val resource = assertIs<ResourceDeclaration>(composition.catalog.show("acceptance.catalog.state", 1))
        val increment = assertIs<ActionDeclaration>(composition.catalog.show("acceptance.catalog.increment", 1))
        val reset = assertIs<ActionDeclaration>(composition.catalog.show("acceptance.catalog.reset", 1))
        assertEquals("sha256:b3ffcc96d1da03c447d243ba196f9af01a38447bbb03d5139aed9923c7300804", resource.valueSchema.digest)
        assertEquals("sha256:bd75453e5e97b497e4ee54db7cd82f4e654d04da370d8106e645e0bb9769c2fe", increment.inputSchema.digest)
        assertEquals("sha256:079b413f2ca72310290f3b07da82503b0dd74c408358570ead26ffeea94efd7c", reset.inputSchema.digest)
        assertEquals(CatalogIdentity("catalog_demo_catalog", 7), composition.catalog.identity)
    }

    @Test
    fun dynamic_resource_availability_and_codec_fail_closed() {
        val composition = createCatalogComposition(processGeneration = 8)
        val resource = assertIs<ResourceDeclaration>(composition.catalog.show("acceptance.catalog.state", 1))
        val increment = assertIs<ActionDeclaration>(composition.catalog.show("acceptance.catalog.increment", 1))
        fun query() = composition.catalog.query(
            ResourceQuery("acceptance.catalog.state", 1, valueSchema = resource.valueSchema),
        )
        fun increment() = composition.actionCoordinator.invoke(
            TargetActionRequest(
                ActionInvocation(
                    increment.id,
                    increment.declarationRevision,
                    increment.inputSchema,
                    buildJsonObject { put("amount", 1) },
                ),
                TargetActionContext("target_acceptance_catalog", 8, "catalog-test"),
                null,
                { true },
            ),
        )

        assertEquals(JsonPrimitive(0), query().value.jsonObject["count"])
        assertEquals(TargetActionResult.COMPLETED, increment())
        assertEquals(
            SemanticFailureKind.UNAVAILABLE,
            runCatching { query() }.exceptionOrNull().let { it as SemanticFailure }.kind,
        )
        assertEquals(TargetActionResult.COMPLETED, increment())
        assertEquals(JsonPrimitive(2), query().value.jsonObject["count"])
        assertEquals(TargetActionResult.COMPLETED, increment())
        assertEquals(
            SemanticFailureKind.DISCLOSURE_DENIED,
            runCatching { query() }.exceptionOrNull().let { it as SemanticFailure }.kind,
        )
        assertEquals(TargetActionResult.COMPLETED, increment())
        assertEquals(
            SemanticFailureKind.RESOURCE_EXHAUSTED,
            runCatching { query() }.exceptionOrNull().let { it as SemanticFailure }.kind,
        )
    }

    @Test
    fun destructive_reset_requires_a_grant_and_current_verification_rejects_it() {
        val composition = createCatalogComposition(processGeneration = 9)
        val reset = assertIs<ActionDeclaration>(composition.catalog.show("acceptance.catalog.reset", 1))
        val failure = runCatching {
            composition.actionCoordinator.invoke(
                TargetActionRequest(
                    ActionInvocation(
                        reset.id,
                        reset.declarationRevision,
                        reset.inputSchema,
                        buildJsonObject { put("confirm", "reset-catalog-v1") },
                    ),
                    TargetActionContext("target_acceptance_catalog", 9, "catalog-test"),
                    "grant",
                    { true },
                ),
            )
        }.exceptionOrNull()
        assertEquals(TargetActionFailureKind.POLICY_DENIED, (failure as TargetActionFailure).kind)
        assertTrue(composition.catalog.identity.generation == 9L)
    }
}
