package dev.apppilotkit.acceptancehost

import dev.apppilotkit.semantic.ResourceQuery
import dev.apppilotkit.semantic.ResourceDeclaration
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import org.junit.Test

class FoundationCompositionTest {
    @Test
    fun cold_process_composition_exposes_the_fixed_foundation_seed() {
        val composition = createFoundationComposition(processGeneration = 41)

        val declaration = composition.catalog.show("acceptance.foundation.state", 1)
        val resource = assertIs<dev.apppilotkit.semantic.ResourceDeclaration>(declaration)
        val result = composition.catalog.query(
            ResourceQuery(
                capability = resource.id,
                declarationRevision = resource.declarationRevision,
                valueSchema = resource.valueSchema,
            ),
        )

        assertEquals(JsonPrimitive("demo.foundation"), result.value.jsonObject["scenario"])
        assertEquals(JsonPrimitive("foundation-v1"), result.value.jsonObject["seed"])
        assertEquals(JsonPrimitive("android"), result.value.jsonObject["platform"])
        assertEquals(41, composition.catalog.identity.generation)
    }

    @Test
    fun foundation_catalog_contains_only_the_read_only_resource() {
        val composition = createFoundationComposition(processGeneration = 1)

        assertEquals(
            listOf("acceptance.foundation.state"),
            composition.catalog.list().map { it.id },
        )
        assertTrue(composition.catalog.list().all { it is ResourceDeclaration })
    }

    @Test
    fun foundation_schema_keeps_platform_open_for_the_shared_contract() {
        val composition = createFoundationComposition(processGeneration = 1)
        val resource = assertIs<ResourceDeclaration>(
            composition.catalog.show("acceptance.foundation.state", 1),
        )

        val document = composition.catalog.schema(
            resource.id,
            resource.declarationRevision,
            resource.valueSchema,
        )
        val platform = document["properties"]!!.jsonObject["platform"]!!.jsonObject

        assertEquals(JsonPrimitive("string"), platform["type"])
        assertTrue("enum" !in platform)
    }

    @Test
    fun foundation_schema_matches_the_shared_contract_document_and_digest() {
        val composition = createFoundationComposition(processGeneration = 1)
        val resource = assertIs<ResourceDeclaration>(
            composition.catalog.show("acceptance.foundation.state", 1),
        )

        val document = composition.catalog.schema(
            resource.id,
            resource.declarationRevision,
            resource.valueSchema,
        )

        assertEquals(
            "{\"\$id\":\"app://acceptance.foundation.state/value@1\",\"\$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"additionalProperties\":false,\"properties\":{\"platform\":{\"type\":\"string\"},\"scenario\":{\"const\":\"demo.foundation\",\"type\":\"string\"},\"seed\":{\"const\":\"foundation-v1\",\"type\":\"string\"}},\"required\":[\"scenario\",\"seed\",\"platform\"],\"type\":\"object\"}",
            Json.encodeToString(document),
        )
        assertEquals(
            "sha256:b63382887a98877b06466df4d6aa2a2fd788e89c5c1db39170720fd6c721a08c",
            resource.valueSchema.digest,
        )
    }
}
