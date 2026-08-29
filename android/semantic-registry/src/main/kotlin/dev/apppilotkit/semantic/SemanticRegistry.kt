package dev.apppilotkit.semantic

import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.math.BigDecimal
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.longOrNull

private const val JSON_SCHEMA_DRAFT_2020_12 = "https://json-schema.org/draft/2020-12/schema"
private val CAPABILITY_ID = Regex("^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$")
private val CATALOG_ID = Regex("^catalog_[A-Za-z0-9._~-]{8,120}$")
private val SCHEMA_ID = Regex("^schema_[A-Za-z0-9._~-]{8,120}$")
private val SCHEMA_URI = Regex("^[a-z][a-z0-9+.-]*:.*$")

enum class SemanticKind { RESOURCE, ACTION }

enum class AuthorizationPolicy { NONE, DESTRUCTIVE_AUTHORIZATION }

enum class RetrySafety { NO_AUTOMATIC_RETRY, RETRY_WITH_PROOF_ONLY }

data class ActionPolicy(
    val authorization: AuthorizationPolicy,
    val retrySafety: RetrySafety,
)

data class CatalogIdentity(val id: String, val generation: Long) {
    internal fun validate() {
        registrationCheck(CATALOG_ID.matches(id))
        registrationCheck(generation >= 1)
    }
}

data class SchemaHandle(val id: String, val revision: Int, val digest: String) {
    internal fun validate() {
        registrationCheck(SCHEMA_ID.matches(id))
        registrationCheck(revision >= 1)
        registrationCheck(Regex("^sha256:[a-f0-9]{64}$").matches(digest))
    }
}

class SemanticSchema private constructor(
    val handle: SchemaHandle,
    val document: JsonObject,
) {
    companion object {
        fun create(id: String, revision: Int, document: JsonObject): SemanticSchema {
            val bytes = try {
                Json.encodeToString(JsonObject.serializer(), document)
                    .toByteArray(StandardCharsets.UTF_8)
            } catch (_: Throwable) {
                throw SemanticFailure(SemanticFailureKind.INVALID_REGISTRATION)
            }
            val detached = try {
                Json.parseToJsonElement(bytes.toString(StandardCharsets.UTF_8)) as JsonObject
            } catch (_: Throwable) {
                throw SemanticFailure(SemanticFailureKind.INVALID_REGISTRATION)
            }
            registrationCheck(detached["\$schema"] == JsonPrimitive(JSON_SCHEMA_DRAFT_2020_12))
            val documentId = (detached["\$id"] as? JsonPrimitive)?.content
            registrationCheck(documentId != null && SCHEMA_URI.matches(documentId))
            registrationCheck(runCatching { SemanticJsonSchema.validateDocument(detached) }.isSuccess)
            val canonicalDocument = try {
                canonicalize(detached) as JsonObject
            } catch (_: Throwable) {
                throw SemanticFailure(SemanticFailureKind.INVALID_REGISTRATION)
            }
            val canonicalBytes = wireJson(canonicalDocument).toByteArray(StandardCharsets.UTF_8)
            val digest = MessageDigest.getInstance("SHA-256")
                .digest(canonicalBytes)
                .joinToString(separator = "", prefix = "sha256:") { "%02x".format(it) }
            val handle = SchemaHandle(id, revision, digest)
            handle.validate()
            return SemanticSchema(handle, canonicalDocument)
        }
    }

    internal fun detachedDocument(): JsonObject = document
}

enum class ClassificationStatus { COMPLETE, UNCLASSIFIED }

enum class RedactionStatus { COMPLETE, INCOMPLETE }

class EncodedSemanticValue(
    utf8: ByteArray,
    val schema: SchemaHandle,
    val classification: ClassificationStatus,
    val redaction: RedactionStatus,
) {
    private val detachedBytes = utf8.copyOf()

    internal fun bytes(): ByteArray = detachedBytes.copyOf()
}

interface SemanticCodec<T> {
    val schema: SemanticSchema

    /** Lets an App adapter fail registration when its generated codec metadata is inconsistent. */
    fun isRegistrationValid(): Boolean = true

    fun decode(value: JsonElement): T

    fun encode(value: T): EncodedSemanticValue

    /** Validates the detached JSON against the schema subset understood by the App adapter. */
    fun validates(value: JsonElement): Boolean
}

sealed interface SemanticDeclaration {
    val id: String
    val kind: SemanticKind
    val declarationRevision: Int
}

data class ResourceDeclaration(
    override val id: String,
    override val declarationRevision: Int,
    val inputSchema: SchemaHandle?,
    val valueSchema: SchemaHandle,
) : SemanticDeclaration {
    override val kind: SemanticKind = SemanticKind.RESOURCE
}

data class ActionDeclaration(
    override val id: String,
    override val declarationRevision: Int,
    val inputSchema: SchemaHandle,
    val policy: ActionPolicy,
) : SemanticDeclaration {
    override val kind: SemanticKind = SemanticKind.ACTION
}

data class ResourceQuery(
    val capability: String,
    val declarationRevision: Int,
    val inputSchema: SchemaHandle? = null,
    val input: JsonElement? = null,
    val valueSchema: SchemaHandle,
)

data class ResourceQueryResult(
    val value: JsonElement,
    val valueSchema: SchemaHandle,
    val bytes: Int,
)

data class ActionInvocation(
    val capability: String,
    val declarationRevision: Int,
    val inputSchema: SchemaHandle,
    val input: JsonElement,
)

enum class SemanticFailureKind(val stockMessage: String) {
    INVALID_REGISTRATION("Semantic registration is invalid."),
    CATALOG_FROZEN("Semantic catalog is frozen."),
    CAPABILITY_NOT_FOUND("Semantic capability was not found."),
    SCHEMA_MISMATCH("Semantic schema does not match."),
    UNAVAILABLE("Semantic capability is unavailable."),
    DISCLOSURE_DENIED("Semantic disclosure was denied."),
    RESOURCE_EXHAUSTED("Semantic response exceeds its limit."),
    HANDLER_FAILED("Semantic handler failed."),
}

class SemanticFailure(val kind: SemanticFailureKind) : RuntimeException(kind.stockMessage)

class SemanticRegistryBuilder {
    private val lock = Any()
    private val entries = linkedMapOf<String, ErasedEntry>()
    private val schemas = linkedMapOf<SchemaKey, SemanticSchema>()
    private var frozen = false

    fun <Value : Any> registerResource(
        id: String,
        declarationRevision: Int,
        valueCodec: SemanticCodec<Value>,
        available: () -> Boolean = { true },
        query: () -> Value,
    ): SemanticRegistryBuilder = registerResourceInternal(
        id = id,
        declarationRevision = declarationRevision,
        inputCodec = null,
        valueCodec = valueCodec,
        available = available,
        query = { query() },
    )

    @Suppress("UNCHECKED_CAST")
    fun <Input : Any, Value : Any> registerResource(
        id: String,
        declarationRevision: Int,
        inputCodec: SemanticCodec<Input>,
        valueCodec: SemanticCodec<Value>,
        available: () -> Boolean = { true },
        query: (Input) -> Value,
    ): SemanticRegistryBuilder = registerResourceInternal(
        id = id,
        declarationRevision = declarationRevision,
        inputCodec = inputCodec,
        valueCodec = valueCodec,
        available = available,
        query = { input -> query(input as Input) },
    )

    @Suppress("UNCHECKED_CAST")
    fun <Input : Any> registerAction(
        id: String,
        declarationRevision: Int,
        inputCodec: SemanticCodec<Input>,
        policy: ActionPolicy,
        available: () -> Boolean = { true },
        invoke: (Input) -> Unit,
    ): SemanticRegistryBuilder = synchronized(lock) {
        checkOpen()
        validateRegistration(id, declarationRevision, inputCodec)
        registrationCheck(id !in entries)
        validateSchemaConflicts(listOf(inputCodec.schema))

        val declaration = ActionDeclaration(id, declarationRevision, inputCodec.schema.handle, policy)
        val entry = ActionEntry(
            declaration = declaration,
            inputCodec = inputCodec.erased(),
            available = available,
            invoke = { input -> invoke(input as Input) },
        )
        entries[id] = entry
        installSchemas(listOf(inputCodec.schema))
        this
    }

    fun freeze(identity: CatalogIdentity, maxDisclosureBytes: Int): SemanticRegistry = synchronized(lock) {
        checkOpen()
        identity.validate()
        registrationCheck(maxDisclosureBytes > 0)
        val snapshot = entries.toMap()
        val schemaSnapshot = schemas.toMap()
        frozen = true
        SemanticRegistry(identity, maxDisclosureBytes, snapshot, schemaSnapshot)
    }

    private fun registerResourceInternal(
        id: String,
        declarationRevision: Int,
        inputCodec: SemanticCodec<*>?,
        valueCodec: SemanticCodec<*>,
        available: () -> Boolean,
        query: (Any?) -> Any,
    ): SemanticRegistryBuilder = synchronized(lock) {
        checkOpen()
        validateRegistration(id, declarationRevision, valueCodec)
        inputCodec?.let { validateRegistration(id, declarationRevision, it) }
        registrationCheck(id !in entries)
        val registrationSchemas = listOfNotNull(inputCodec?.schema, valueCodec.schema)
        validateSchemaConflicts(registrationSchemas)

        val declaration = ResourceDeclaration(
            id = id,
            declarationRevision = declarationRevision,
            inputSchema = inputCodec?.schema?.handle,
            valueSchema = valueCodec.schema.handle,
        )
        entries[id] = ResourceEntry(
            declaration = declaration,
            inputCodec = inputCodec?.erased(),
            valueCodec = valueCodec.erased(),
            available = available,
            query = query,
        )
        installSchemas(registrationSchemas)
        this
    }

    private fun validateRegistration(id: String, revision: Int, codec: SemanticCodec<*>) {
        registrationCheck(CAPABILITY_ID.matches(id) && id.length <= 128)
        registrationCheck(revision >= 1)
        registrationCheck(runCatching { codec.isRegistrationValid() }.getOrDefault(false))
        codec.schema.handle.validate()
    }

    private fun validateSchemaConflicts(candidates: List<SemanticSchema>) {
        val combined = linkedMapOf<SchemaKey, SemanticSchema>()
        for (schema in candidates) {
            val key = SchemaKey(schema.handle.id, schema.handle.revision)
            val existing = combined[key] ?: schemas[key]
            registrationCheck(existing == null || existing.handle == schema.handle)
            combined[key] = schema
        }
    }

    private fun installSchemas(candidates: List<SemanticSchema>) {
        for (schema in candidates) {
            schemas.putIfAbsent(SchemaKey(schema.handle.id, schema.handle.revision), schema)
        }
    }

    private fun checkOpen() {
        if (frozen) throw SemanticFailure(SemanticFailureKind.CATALOG_FROZEN)
    }
}

class SemanticRegistry internal constructor(
    val identity: CatalogIdentity,
    private val maxDisclosureBytes: Int,
    private val entries: Map<String, ErasedEntry>,
    private val schemas: Map<SchemaKey, SemanticSchema>,
) {
    fun list(): List<SemanticDeclaration> = entries.values
        .map { it.declaration }
        .sortedBy { it.id }

    fun show(capability: String, declarationRevision: Int): SemanticDeclaration {
        val entry = matchingEntry(capability, declarationRevision)
        return entry.declaration
    }

    fun schema(
        capability: String,
        declarationRevision: Int,
        handle: SchemaHandle,
    ): JsonObject {
        val entry = matchingEntry(capability, declarationRevision)
        val declaredHandles = when (entry) {
            is ResourceEntry -> listOfNotNull(
                entry.inputCodec?.schema?.handle,
                entry.valueCodec.schema.handle,
            )
            is ActionEntry -> listOf(entry.inputCodec.schema.handle)
        }
        if (handle !in declaredHandles) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }
        val schema = schemas[SchemaKey(handle.id, handle.revision)]
            ?.takeIf { candidate -> candidate.handle == handle }
            ?: throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        val document = schema.detachedDocument()
        val bytes = wireJson(document).toByteArray(StandardCharsets.UTF_8)
        if (bytes.size > maxDisclosureBytes) {
            throw SemanticFailure(SemanticFailureKind.RESOURCE_EXHAUSTED)
        }
        return document
    }

    fun query(request: ResourceQuery): ResourceQueryResult {
        val entry = matchingEntry(request.capability, request.declarationRevision) as? ResourceEntry
            ?: throw SemanticFailure(SemanticFailureKind.CAPABILITY_NOT_FOUND)
        if (request.valueSchema != entry.declaration.valueSchema) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }

        val input = decodeResourceInput(entry, request)
        if (!safeAvailability(entry.available)) {
            throw SemanticFailure(SemanticFailureKind.UNAVAILABLE)
        }
        val typedValue = try {
            entry.query(input)
        } catch (_: Throwable) {
            throw SemanticFailure(SemanticFailureKind.HANDLER_FAILED)
        }
        val encoded = try {
            entry.valueCodec.encode(typedValue)
        } catch (_: Throwable) {
            throw SemanticFailure(SemanticFailureKind.DISCLOSURE_DENIED)
        }
        return disclose(encoded, entry.valueCodec, entry.declaration.valueSchema)
    }

    internal fun prepareAction(request: ActionInvocation): PreparedAction {
        val entry = matchingEntry(request.capability, request.declarationRevision) as? ActionEntry
            ?: throw SemanticFailure(SemanticFailureKind.CAPABILITY_NOT_FOUND)
        if (request.inputSchema != entry.declaration.inputSchema) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }
        val typedInput = safeDecode(entry.inputCodec, request.input)
        if (!safeAvailability(entry.available)) {
            throw SemanticFailure(SemanticFailureKind.UNAVAILABLE)
        }
        return PreparedAction(entry.declaration, typedInput, entry.invoke)
    }

    private fun decodeResourceInput(entry: ResourceEntry, request: ResourceQuery): Any? {
        val codec = entry.inputCodec
        if (codec == null) {
            if (request.inputSchema != null || request.input != null) {
                throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
            }
            return null
        }
        if (request.inputSchema != codec.schema.handle || request.input == null) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }
        return safeDecode(codec, request.input)
    }

    private fun disclose(
        encoded: EncodedSemanticValue,
        codec: ErasedCodec,
        declaredSchema: SchemaHandle,
    ): ResourceQueryResult {
        if (encoded.classification != ClassificationStatus.COMPLETE ||
            encoded.redaction != RedactionStatus.COMPLETE
        ) {
            throw SemanticFailure(SemanticFailureKind.DISCLOSURE_DENIED)
        }
        if (encoded.schema != declaredSchema || codec.schema.handle != declaredSchema) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }
        val bytes = encoded.bytes()
        if (bytes.size > maxDisclosureBytes) {
            throw SemanticFailure(SemanticFailureKind.RESOURCE_EXHAUSTED)
        }
        val text = decodeUtf8(bytes)
        val value = try {
            canonicalize(Json.parseToJsonElement(text))
        } catch (_: Throwable) {
            throw SemanticFailure(SemanticFailureKind.DISCLOSURE_DENIED)
        }
        val valid = try {
            SemanticJsonSchema.validate(value, codec.schema.document)
            codec.validates(value)
        } catch (_: Throwable) {
            false
        }
        if (!valid) throw SemanticFailure(SemanticFailureKind.DISCLOSURE_DENIED)
        val canonicalBytes = wireJson(value).toByteArray(StandardCharsets.UTF_8)
        if (canonicalBytes.size > maxDisclosureBytes) {
            throw SemanticFailure(SemanticFailureKind.RESOURCE_EXHAUSTED)
        }
        return ResourceQueryResult(value, declaredSchema, canonicalBytes.size)
    }

    private fun matchingEntry(capability: String, declarationRevision: Int): ErasedEntry {
        val entry = entries[capability]
            ?: throw SemanticFailure(SemanticFailureKind.CAPABILITY_NOT_FOUND)
        if (entry.declaration.declarationRevision != declarationRevision) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }
        return entry
    }
}

internal class PreparedAction(
    val declaration: ActionDeclaration,
    private val typedInput: Any,
    private val invoke: (Any) -> Unit,
) {
    fun dispatch() {
        try {
            invoke(typedInput)
        } catch (_: Throwable) {
            throw SemanticFailure(SemanticFailureKind.HANDLER_FAILED)
        }
    }
}

internal data class SchemaKey(val id: String, val revision: Int)

internal sealed interface ErasedEntry {
    val declaration: SemanticDeclaration
    val available: () -> Boolean
}

internal data class ResourceEntry(
    override val declaration: ResourceDeclaration,
    val inputCodec: ErasedCodec?,
    val valueCodec: ErasedCodec,
    override val available: () -> Boolean,
    val query: (Any?) -> Any,
) : ErasedEntry

internal data class ActionEntry(
    override val declaration: ActionDeclaration,
    val inputCodec: ErasedCodec,
    override val available: () -> Boolean,
    val invoke: (Any) -> Unit,
) : ErasedEntry

internal class ErasedCodec(private val codec: SemanticCodec<Any>) {
    val schema: SemanticSchema get() = codec.schema
    fun decode(value: JsonElement): Any = codec.decode(value)
    fun encode(value: Any): EncodedSemanticValue = codec.encode(value)
    fun validates(value: JsonElement): Boolean = codec.validates(value)
}

@Suppress("UNCHECKED_CAST")
private fun SemanticCodec<*>.erased(): ErasedCodec = ErasedCodec(this as SemanticCodec<Any>)

private fun safeDecode(codec: ErasedCodec, value: JsonElement): Any {
    val valid = try {
        SemanticJsonSchema.validate(value, codec.schema.document)
        codec.validates(value)
    } catch (_: Throwable) {
        false
    }
    if (!valid) throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
    return try {
        codec.decode(value)
    } catch (_: Throwable) {
        throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
    }
}

private fun safeAvailability(available: () -> Boolean): Boolean = try {
    available()
} catch (_: Throwable) {
    false
}

private fun decodeUtf8(bytes: ByteArray): String = try {
    StandardCharsets.UTF_8.newDecoder()
        .onMalformedInput(CodingErrorAction.REPORT)
        .onUnmappableCharacter(CodingErrorAction.REPORT)
        .decode(ByteBuffer.wrap(bytes))
        .toString()
} catch (_: Throwable) {
    throw SemanticFailure(SemanticFailureKind.DISCLOSURE_DENIED)
}

private fun registrationCheck(condition: Boolean) {
    if (!condition) throw SemanticFailure(SemanticFailureKind.INVALID_REGISTRATION)
}

private fun canonicalize(value: JsonElement): JsonElement = when (value) {
    is JsonObject -> JsonObject(
        value.entries.sortedBy { it.key }.associate { (key, child) -> key to canonicalize(child) },
    )
    is JsonArray -> JsonArray(value.map(::canonicalize))
    is JsonPrimitive -> when {
        value.isString || value.booleanOrNull != null || value === JsonNull -> value
        else -> Json.parseToJsonElement(ecmaNumber(value.content))
    }
}

private fun wireJson(value: JsonElement): String = when (value) {
    JsonNull -> "null"
    is JsonObject -> value.entries.joinToString(prefix = "{", postfix = "}") { (key, child) ->
        val encodedKey = Json.encodeToString(JsonPrimitive.serializer(), JsonPrimitive(key))
        "$encodedKey:${wireJson(child)}"
    }
    is JsonArray -> value.joinToString(prefix = "[", postfix = "]") { wireJson(it) }
    is JsonPrimitive -> when {
        value.isString -> Json.encodeToString(JsonPrimitive.serializer(), value)
        value.booleanOrNull != null -> value.content
        else -> ecmaNumber(value.content)
    }
}

private fun ecmaNumber(raw: String): String {
    val number = raw.toDoubleOrNull()?.takeIf { it.isFinite() }
        ?: throw SemanticFailure(SemanticFailureKind.DISCLOSURE_DENIED)
    if (number == 0.0) return "0"
    val absolute = kotlin.math.abs(number)
    val decimal = BigDecimal.valueOf(number).stripTrailingZeros()
    if (absolute >= 1e-6 && absolute < 1e21) return decimal.toPlainString()

    val javaScientific = number.toString()
    if ('E' in javaScientific || 'e' in javaScientific) {
        val parts = javaScientific.lowercase().split('e', limit = 2)
        val mantissa = parts[0].removeSuffix(".0")
        val exponent = parts[1].toInt()
        val sign = if (exponent >= 0) "+" else ""
        return "${mantissa}e$sign$exponent"
    }

    val digits = decimal.unscaledValue().abs().toString()
    val exponent = digits.length - decimal.scale() - 1
    val signPrefix = if (decimal.signum() < 0) "-" else ""
    val mantissa = if (digits.length == 1) digits else "${digits[0]}.${digits.drop(1)}"
    val exponentSign = if (exponent >= 0) "+" else ""
    return "$signPrefix${mantissa}e$exponentSign$exponent"
}

private object SemanticJsonSchema {
    private val supportedKeywords = setOf(
        "\$schema", "\$id", "title", "description", "deprecated", "readOnly", "writeOnly",
        "type", "enum", "const", "required", "properties", "additionalProperties", "items",
        "minLength", "maxLength", "minimum", "maximum", "minItems", "maxItems",
    )
    private val supportedTypes = setOf(
        "null", "boolean", "integer", "number", "string", "array", "object",
    )

    fun validateDocument(document: JsonObject) {
        validateSchemaObject(document, isRoot = true)
    }

    fun validate(value: JsonElement, document: JsonObject) {
        validateValue(value, document)
    }

    private fun validateSchemaObject(schema: JsonObject, isRoot: Boolean = false) {
        schemaCheck(schema.keys.all { it in supportedKeywords })
        if (!isRoot) schemaCheck("\$schema" !in schema && "\$id" !in schema)
        val type = schema.string("type")
        schemaCheck(type in supportedTypes)

        schema["enum"]?.let { value ->
            val values = value as? JsonArray
            schemaCheck(values != null && values.isNotEmpty())
        }

        when (type) {
            "object" -> validateObjectSchema(schema)
            "array" -> validateArraySchema(schema)
            "string" -> validateStringSchema(schema)
            "integer", "number" -> validateNumberSchema(schema)
        }

        val objectOnly = setOf("required", "properties", "additionalProperties")
        val arrayOnly = setOf("items", "minItems", "maxItems")
        val stringOnly = setOf("minLength", "maxLength")
        val numberOnly = setOf("minimum", "maximum")
        schemaCheck(type == "object" || objectOnly.none { it in schema })
        schemaCheck(type == "array" || arrayOnly.none { it in schema })
        schemaCheck(type == "string" || stringOnly.none { it in schema })
        schemaCheck(type == "integer" || type == "number" || numberOnly.none { it in schema })
    }

    private fun validateObjectSchema(schema: JsonObject) {
        schemaCheck((schema["additionalProperties"] as? JsonPrimitive)?.booleanOrNull == false)
        val properties = when (val value = schema["properties"]) {
            null -> JsonObject(emptyMap())
            is JsonObject -> value
            else -> schemaError()
        }
        properties.values.forEach { property ->
            schemaCheck(property is JsonObject)
            validateSchemaObject(property as JsonObject)
        }
        schema["required"]?.let { value ->
            val required = value as? JsonArray ?: schemaError()
            val names = required.map { element ->
                val primitive = element as? JsonPrimitive
                schemaCheck(primitive?.isString == true)
                primitive!!.content
            }
            schemaCheck(names.toSet().size == names.size)
            schemaCheck(names.all { it in properties })
        }
    }

    private fun validateArraySchema(schema: JsonObject) {
        val items = schema["items"] as? JsonObject ?: schemaError()
        validateSchemaObject(items)
        validateNonnegativeInteger(schema["minItems"])
        validateNonnegativeInteger(schema["maxItems"])
        validateOrderedIntegerBounds(schema["minItems"], schema["maxItems"])
    }

    private fun validateStringSchema(schema: JsonObject) {
        validateNonnegativeInteger(schema["minLength"])
        validateNonnegativeInteger(schema["maxLength"])
        validateOrderedIntegerBounds(schema["minLength"], schema["maxLength"])
    }

    private fun validateNumberSchema(schema: JsonObject) {
        schema["minimum"]?.let { schemaCheck(it.decimalOrNull() != null) }
        schema["maximum"]?.let { schemaCheck(it.decimalOrNull() != null) }
        val minimum = schema["minimum"]?.decimalOrNull()
        val maximum = schema["maximum"]?.decimalOrNull()
        schemaCheck(minimum == null || maximum == null || minimum <= maximum)
    }

    private fun validateValue(value: JsonElement, schema: JsonObject) {
        schema["const"]?.let { schemaCheck(value == it) }
        (schema["enum"] as? JsonArray)?.let { schemaCheck(value in it) }
        when (schema.string("type")) {
            "null" -> schemaCheck(value === JsonNull)
            "boolean" -> schemaCheck((value as? JsonPrimitive)?.booleanOrNull != null)
            "integer" -> {
                schemaCheck(value.isInteger())
                validateNumericValue(value, schema)
            }
            "number" -> {
                schemaCheck(value.decimalOrNull() != null)
                validateNumericValue(value, schema)
            }
            "string" -> validateStringValue(value, schema)
            "array" -> validateArrayValue(value, schema)
            "object" -> validateObjectValue(value, schema)
            else -> schemaError()
        }
    }

    private fun validateStringValue(value: JsonElement, schema: JsonObject) {
        val primitive = value as? JsonPrimitive
        schemaCheck(primitive?.isString == true)
        val string = primitive!!.content
        val count = string.codePointCount(0, string.length)
        schema["minLength"]?.nonnegativeIntOrNull()?.let { schemaCheck(count >= it) }
        schema["maxLength"]?.nonnegativeIntOrNull()?.let { schemaCheck(count <= it) }
    }

    private fun validateArrayValue(value: JsonElement, schema: JsonObject) {
        val values = value as? JsonArray ?: schemaError()
        schema["minItems"]?.nonnegativeIntOrNull()?.let { schemaCheck(values.size >= it) }
        schema["maxItems"]?.nonnegativeIntOrNull()?.let { schemaCheck(values.size <= it) }
        val items = schema["items"] as? JsonObject ?: schemaError()
        values.forEach { validateValue(it, items) }
    }

    private fun validateObjectValue(value: JsonElement, schema: JsonObject) {
        val fields = value as? JsonObject ?: schemaError()
        val properties = schema["properties"] as? JsonObject ?: JsonObject(emptyMap())
        schemaCheck(fields.keys.all { it in properties })
        (schema["required"] as? JsonArray)?.forEach { required ->
            schemaCheck((required as JsonPrimitive).content in fields)
        }
        fields.forEach { (name, field) ->
            val fieldSchema = properties[name] as? JsonObject ?: schemaError()
            validateValue(field, fieldSchema)
        }
    }

    private fun validateNumericValue(value: JsonElement, schema: JsonObject) {
        val number = value.decimalOrNull() ?: schemaError()
        schema["minimum"]?.decimalOrNull()?.let { schemaCheck(number >= it) }
        schema["maximum"]?.decimalOrNull()?.let { schemaCheck(number <= it) }
    }

    private fun validateNonnegativeInteger(value: JsonElement?) {
        if (value != null) schemaCheck(value.nonnegativeIntOrNull() != null)
    }

    private fun validateOrderedIntegerBounds(minimum: JsonElement?, maximum: JsonElement?) {
        val min = minimum?.nonnegativeIntOrNull()
        val max = maximum?.nonnegativeIntOrNull()
        schemaCheck(min == null || max == null || min <= max)
    }

    private fun JsonObject.string(key: String): String? {
        val primitive = this[key] as? JsonPrimitive
        return primitive?.takeIf { it.isString }?.contentOrNull
    }

    private fun JsonElement.isInteger(): Boolean {
        val primitive = this as? JsonPrimitive ?: return false
        return !primitive.isString && primitive.longOrNull != null
    }

    private fun JsonElement.nonnegativeIntOrNull(): Int? {
        val value = (this as? JsonPrimitive)?.takeIf { !it.isString }?.longOrNull ?: return null
        return value.takeIf { it in 0..Int.MAX_VALUE }?.toInt()
    }

    private fun JsonElement.decimalOrNull(): BigDecimal? {
        val primitive = this as? JsonPrimitive ?: return null
        if (primitive.isString || primitive.booleanOrNull != null) return null
        return runCatching { primitive.content.toBigDecimal() }
            .getOrNull()
            ?.takeIf { it.toDouble().isFinite() }
    }

    private fun schemaCheck(condition: Boolean) {
        if (!condition) schemaError()
    }

    private fun schemaError(): Nothing = throw IllegalArgumentException("invalid schema value")
}
