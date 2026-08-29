package dev.apppilotkit.semantic.runtime

import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.security.SecureRandom
import dev.apppilotkit.semantic.ActionDeclaration
import dev.apppilotkit.semantic.AuthorizationPolicy
import dev.apppilotkit.semantic.ResourceDeclaration
import dev.apppilotkit.semantic.ResourceQuery
import dev.apppilotkit.semantic.RetrySafety
import dev.apppilotkit.semantic.SchemaHandle
import dev.apppilotkit.semantic.SemanticDeclaration
import dev.apppilotkit.semantic.SemanticFailure
import dev.apppilotkit.semantic.SemanticFailureKind
import dev.apppilotkit.semantic.SemanticKind
import dev.apppilotkit.semantic.SemanticRegistry
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import kotlinx.serialization.json.putJsonObject

/** Fixed bounds which the transport-independent runtime enforces on raw messages. */
data class ProtocolRuntimeLimits(
    val maxRequestBytes: Int,
    val maxResponseBytes: Int,
    val maxPageItems: Int,
) {
    init {
        require(maxRequestBytes in 1_024..16 * 1024 * 1024)
        require(maxResponseBytes in 1_024..64 * 1024 * 1024)
        require(maxPageItems in 1..10_000)
    }
}

data class ProtocolSessionContext(val id: String, val generation: Long)

/** App-owned gates. Callers must inject every gate; there is intentionally no permissive default. */
data class SemanticProtocolPolicy(
    val discover: (ProtocolSessionContext, SemanticDeclaration) -> Boolean,
    val discloseSchema: (ProtocolSessionContext, SemanticDeclaration) -> Boolean,
    val discloseResource: (ProtocolSessionContext, ResourceDeclaration) -> Boolean,
)

/**
 * Raw UTF-8 JSON-RPC implementation for the v1 semantic catalog family.
 * Authentication, framing, listener ownership, and action coordination remain outside this class.
 */
class ProtocolRuntime(
    private val catalog: SemanticRegistry,
    private val limits: ProtocolRuntimeLimits,
    private val policy: SemanticProtocolPolicy,
) {
    private data class Session(
        val context: ProtocolSessionContext,
        val minor: Int,
        val capabilities: Set<String>,
    )

    private data class RequestedListLimits(val maxItems: Int?, val maxBytes: Int?)

    private data class Cursor(
        val context: ProtocolSessionContext,
        val catalogId: String,
        val catalogGeneration: Long,
        val method: String,
        val originalLimits: RequestedListLimits,
        val visibleDeclarations: List<SemanticDeclaration>,
        val nextIndex: Int,
    )

    private class Fault(
        val code: Int,
        val kind: String,
        val stockMessage: String,
        val retryable: Boolean,
    ) : RuntimeException(stockMessage)

    private val stateLock = Any()
    private val sessions = mutableMapOf<String, Session>()
    private val cursors = mutableMapOf<String, Cursor>()
    private val random = SecureRandom()

    /** Invalidates listener-bound session and cursor state without changing the frozen Catalog. */
    fun invalidateSessions() = synchronized(stateLock) {
        sessions.clear()
        cursors.clear()
    }

    /** Handles exactly one complete UTF-8 JSON-RPC message. */
    fun handle(bytes: ByteArray): ByteArray {
        val raw = try {
            JSON.parseToJsonElement(strictUtf8(bytes))
        } catch (_: Throwable) {
            return error(JsonNull, PARSE_ERROR)
        }
        val id = requestId(raw) ?: JsonNull
        if (bytes.size > limits.maxRequestBytes) return error(id, RESOURCE_EXHAUSTED)
        return try {
            val encoded = encode(dispatch(raw))
            if (encoded.size > limits.maxResponseBytes) error(id, RESOURCE_EXHAUSTED) else encoded
        } catch (fault: Fault) {
            error(id, fault)
        } catch (_: Throwable) {
            error(id, INTERNAL_ERROR)
        }
    }

    private fun dispatch(raw: JsonElement): JsonObject {
        val envelope = raw as? JsonObject ?: throw INVALID_REQUEST
        requireKeys(envelope, setOf("jsonrpc", "id", "method", "params", "context"), INVALID_REQUEST)
        if (envelope["jsonrpc"] != JsonPrimitive("2.0") ||
            string(envelope["id"], 128).isNullOrEmpty() ||
            validMethod(envelope["method"]) == null
        ) throw INVALID_REQUEST
        val id = JsonPrimitive(string(envelope["id"], 128)!!)
        val method = validMethod(envelope["method"])!!

        if (method == "session.open") {
            if ("context" in envelope) throw INVALID_REQUEST
            return open(id, envelope["params"] as? JsonObject ?: throw INVALID_PARAMS)
        }
        val context = parseContext(envelope["context"] as? JsonObject ?: throw INVALID_REQUEST)
        val session = synchronized(stateLock) {
            sessions[context.id]?.takeIf { it.context == context }
        } ?: throw SESSION_EXPIRED
        val params = envelope["params"] as? JsonObject ?: throw INVALID_PARAMS

        if (method.startsWith("semantic.")) {
            if (session.minor < 2) throw METHOD_NOT_FOUND
            if (SEMANTIC_CATALOG !in session.capabilities) throw CAPABILITY_UNAVAILABLE
        }
        return when (method) {
            "semantic.list" -> list(id, session, params)
            "semantic.show" -> show(id, session, params)
            "semantic.schema" -> schema(id, session, params)
            "semantic.query" -> query(id, session, params)
            else -> throw METHOD_NOT_FOUND
        }
    }

    private fun open(id: JsonPrimitive, params: JsonObject): JsonObject {
        requireKeys(params, setOf("client", "protocol", "requiredCapabilities"), INVALID_PARAMS)
        if (!validClient(params["client"] as? JsonObject)) throw INVALID_PARAMS
        val minor = selectMinor(params["protocol"] as? JsonObject ?: throw INVALID_PARAMS)
        val required = requestedCapabilities(params["requiredCapabilities"])
        val offered = if (minor == 2) setOf(CORE, SEMANTIC_CATALOG) else setOf(CORE)
        if (!offered.containsAll(required)) throw CAPABILITY_UNAVAILABLE
        val advertised = if (SEMANTIC_CATALOG in required) offered else setOf(CORE)

        val context = ProtocolSessionContext(newSessionId(), catalog.identity.generation)
        val session = Session(context, minor, advertised)
        synchronized(stateLock) { sessions[context.id] = session }
        return success(id, buildJsonObject {
            putJsonObject("context") {
                put("id", context.id)
                put("generation", context.generation)
            }
            putJsonObject("protocol") {
                put("major", 1)
                put("minor", minor)
            }
            putJsonArray("capabilities") { advertised.sorted().forEach { add(JsonPrimitive(it)) } }
            putJsonObject("limits") {
                put("maxRequestBytes", limits.maxRequestBytes)
                put("maxResponseBytes", limits.maxResponseBytes)
                put("maxPageItems", limits.maxPageItems)
            }
        })
    }

    private fun list(id: JsonPrimitive, session: Session, params: JsonObject): JsonObject {
        ensureActive(session)
        val start: Int
        val requested: RequestedListLimits
        val visible: List<SemanticDeclaration>
        val cursorToken = params["cursor"]
        if (cursorToken != null) {
            requireKeys(params, setOf("cursor"), INVALID_PARAMS)
            val token = string(cursorToken, 4_096)?.takeIf { it.isNotEmpty() } ?: throw INVALID_PARAMS
            val cursor = synchronized(stateLock) {
                val candidate = cursors[token] ?: throw INVALID_PARAMS
                if (candidate.context != session.context || candidate.method != "semantic.list") {
                    throw INVALID_PARAMS
                }
                if (candidate.catalogId != catalog.identity.id ||
                    candidate.catalogGeneration != catalog.identity.generation
                ) {
                    throw CURSOR_EXPIRED
                }
                cursors.remove(token)
                candidate
            }
            start = cursor.nextIndex
            requested = cursor.originalLimits
            visible = cursor.visibleDeclarations
        } else {
            requireKeys(params, setOf("limits"), INVALID_PARAMS)
            start = 0
            requested = requestedLimits(params["limits"])
            ensureActive(session)
            visible = catalog.list().filter { permitsDiscovery(session.context, it) }
        }
        val maxItems = minOf(requested.maxItems ?: limits.maxPageItems, limits.maxPageItems)
        val maxBytes = minOf(requested.maxBytes ?: limits.maxResponseBytes, limits.maxResponseBytes)
        if (start > visible.size) throw CURSOR_EXPIRED

        var end = minOf(start + maxItems, visible.size)
        var byteLimited = false
        var response: JsonObject
        var next: String? = null
        while (true) {
            val hasMore = end < visible.size
            val proposed = if (hasMore) newCursorToken() else null
            response = listResponse(id, visible.subList(start, end), maxItems, maxBytes, proposed, byteLimited)
            val responseBytes = encode(response).size
            if (responseBytes > limits.maxResponseBytes) throw RESOURCE_EXHAUSTED
            if (responseBytes <= maxBytes) {
                next = proposed
                break
            }
            if (end == start) throw RESOURCE_EXHAUSTED
            end -= 1
            byteLimited = true
        }
        synchronized(stateLock) {
            ensureActiveLocked(session)
            if (next != null) {
                cursors[next] = Cursor(
                    session.context,
                    catalog.identity.id,
                    catalog.identity.generation,
                    "semantic.list",
                    requested,
                    visible,
                    end,
                )
            }
        }
        return response
    }

    private fun show(id: JsonPrimitive, session: Session, params: JsonObject): JsonObject {
        val (capability, revision) = capabilityRevision(params)
        ensureActive(session)
        val declaration = discoveredDeclaration(session.context, capability, revision)
        return activeSuccess(session, id) { declarationValue(declaration) }
    }

    private fun schema(id: JsonPrimitive, session: Session, params: JsonObject): JsonObject {
        requireKeys(params, setOf("capability", "declarationRevision", "schema"), INVALID_PARAMS)
        val capability = capability(params["capability"])
        val revision = positiveLong(params["declarationRevision"])
        val handle = schemaHandle(params["schema"])
        ensureActive(session)
        val declaration = discoveredDeclaration(session.context, capability, revision)
        if (!declaredHandles(declaration).contains(handle)) throw semanticSchemaMismatch()
        ensureActive(session)
        if (!permitsSchema(session.context, declaration)) throw semanticDisclosureDenied()
        val document = try {
            ensureActive(session)
            catalog.schema(capability, revision.toInt(), handle)
        } catch (failure: SemanticFailure) {
            throw mapFailure(failure)
        }
        return activeSuccess(session, id) { buildJsonObject {
            put("schema", schemaValue(handle))
            put("document", document)
        } }
    }

    private fun query(id: JsonPrimitive, session: Session, params: JsonObject): JsonObject {
        requireKeys(params, setOf("capability", "declarationRevision", "inputSchema", "input", "valueSchema"), INVALID_PARAMS)
        val capability = capability(params["capability"])
        val revision = positiveLong(params["declarationRevision"])
        if (revision > Int.MAX_VALUE) throw semanticSchemaMismatch()
        val inputSchema = params["inputSchema"]?.let(::schemaHandle)
        val input = params["input"]
        val valueSchema = schemaHandle(params["valueSchema"])
        if ((inputSchema == null) != (input == null)) throw INVALID_PARAMS
        ensureActive(session)
        val declaration = discoveredDeclaration(session.context, capability, revision)
        val resource = declaration as? ResourceDeclaration ?: throw semanticCapabilityNotFound()
        if (resource.valueSchema != valueSchema || resource.inputSchema != inputSchema) throw semanticSchemaMismatch()
        ensureActive(session)
        if (!permitsResource(session.context, resource)) throw semanticDisclosureDenied()
        val result = try {
            ensureActive(session)
            catalog.query(ResourceQuery(capability, revision.toInt(), inputSchema, input, valueSchema))
        } catch (failure: SemanticFailure) {
            throw mapFailure(failure)
        }
        return activeSuccess(session, id) { buildJsonObject {
            put("value", result.value)
            put("valueSchema", schemaValue(result.valueSchema))
            put("bytes", result.bytes)
        } }
    }

    private fun discoveredDeclaration(
        context: ProtocolSessionContext,
        capability: String,
        revision: Long,
    ): SemanticDeclaration {
        val declaration = catalog.list().firstOrNull { it.id == capability } ?: throw semanticCapabilityNotFound()
        if (!permitsDiscovery(context, declaration)) throw semanticCapabilityNotFound()
        if (revision > Int.MAX_VALUE || declaration.declarationRevision.toLong() != revision) {
            throw semanticSchemaMismatch()
        }
        return declaration
    }

    private fun permitsDiscovery(context: ProtocolSessionContext, declaration: SemanticDeclaration): Boolean =
        runCatching { policy.discover(context, declaration) }.getOrDefault(false)

    private fun permitsSchema(context: ProtocolSessionContext, declaration: SemanticDeclaration): Boolean =
        runCatching { policy.discloseSchema(context, declaration) }.getOrDefault(false)

    private fun permitsResource(context: ProtocolSessionContext, declaration: ResourceDeclaration): Boolean =
        runCatching { policy.discloseResource(context, declaration) }.getOrDefault(false)

    private fun ensureActive(session: Session) = synchronized(stateLock) { ensureActiveLocked(session) }

    private fun ensureActiveLocked(session: Session) {
        if (sessions[session.context.id] != session) throw SESSION_EXPIRED
    }

    private fun activeSuccess(
        session: Session,
        id: JsonElement,
        result: () -> JsonElement,
    ): JsonObject = synchronized(stateLock) {
        ensureActiveLocked(session)
        success(id, result())
    }

    private fun selectMinor(range: JsonObject): Int {
        requireKeys(range, setOf("major", "minMinor", "maxMinor"), INVALID_PARAMS)
        val major = positiveLong(range["major"])
        val minimum = nonnegativeInt(range["minMinor"])
        val maximum = nonnegativeInt(range["maxMinor"])
        if (major != 1L || minimum > maximum || minimum > 2) throw INCOMPATIBLE_PROTOCOL
        return minOf(maximum, 2)
    }

    private fun requestedCapabilities(value: JsonElement?): Set<String> {
        if (value == null) return emptySet()
        val array = value as? JsonArray ?: throw INVALID_PARAMS
        return buildSet {
            for (element in array) {
                val capability = string(element, 128)?.takeIf { SESSION_CAPABILITY.matches(it) } ?: throw INVALID_PARAMS
                if (!add(capability)) throw INVALID_PARAMS
            }
        }
    }

    private fun requestedLimits(value: JsonElement?): RequestedListLimits {
        if (value == null) return RequestedListLimits(null, null)
        val objectValue = value as? JsonObject ?: throw INVALID_PARAMS
        requireKeys(objectValue, setOf("maxItems", "maxBytes"), INVALID_PARAMS)
        if (objectValue.isEmpty()) throw INVALID_PARAMS
        val items = objectValue["maxItems"]?.let(::positiveInt)
        val bytes = objectValue["maxBytes"]?.let(::positiveInt)
        if (items != null && items !in 1..10_000 || bytes != null && bytes !in 1_024..64 * 1024 * 1024) {
            throw INVALID_PARAMS
        }
        return RequestedListLimits(items, bytes)
    }

    private fun capabilityRevision(params: JsonObject): Pair<String, Long> {
        requireKeys(params, setOf("capability", "declarationRevision"), INVALID_PARAMS)
        return capability(params["capability"]) to positiveLong(params["declarationRevision"])
    }

    private fun capability(value: JsonElement?): String = validCapability(value) ?: throw INVALID_PARAMS

    private fun schemaHandle(value: JsonElement?): SchemaHandle {
        val objectValue = value as? JsonObject ?: throw INVALID_PARAMS
        requireKeys(objectValue, setOf("id", "revision", "digest"), INVALID_PARAMS)
        val schemaId = string(objectValue["id"], 128)
            ?.takeIf { SCHEMA_ID.matches(it) } ?: throw INVALID_PARAMS
        val revision = positiveLong(objectValue["revision"])
        if (revision > Int.MAX_VALUE) throw INVALID_PARAMS
        val digest = string(objectValue["digest"], 71)
            ?.takeIf { DIGEST.matches(it) } ?: throw INVALID_PARAMS
        return SchemaHandle(schemaId, revision.toInt(), digest)
    }

    private fun parseContext(value: JsonObject): ProtocolSessionContext {
        requireKeys(value, setOf("id", "generation"), INVALID_REQUEST)
        val id = string(value["id"], 128)?.takeIf { SESSION_ID.matches(it) } ?: throw INVALID_REQUEST
        return ProtocolSessionContext(id, positiveLong(value["generation"], INVALID_REQUEST))
    }

    private fun validClient(value: JsonObject?): Boolean = value != null && runCatching {
        requireKeys(value, setOf("name", "version"), INVALID_PARAMS)
        !string(value["name"], 128).isNullOrEmpty() && !string(value["version"], 64).isNullOrEmpty()
    }.getOrDefault(false)

    private fun validCapability(value: JsonElement?): String? =
        string(value, 128)?.takeIf { CAPABILITY_ID.matches(it) }

    private fun validMethod(value: JsonElement?): String? =
        string(value, 128)?.takeIf { METHOD.matches(it) }

    private fun positiveInt(value: JsonElement?): Int {
        val number = (value as? JsonPrimitive)?.takeIf { !it.isString }?.longOrNull ?: throw INVALID_PARAMS
        if (number !in 1..Int.MAX_VALUE) throw INVALID_PARAMS
        return number.toInt()
    }

    private fun positiveLong(value: JsonElement?, fault: Fault = INVALID_PARAMS): Long {
        val number = (value as? JsonPrimitive)?.takeIf { !it.isString }?.longOrNull ?: throw fault
        if (number < 1) throw fault
        return number
    }

    private fun nonnegativeInt(value: JsonElement?): Int {
        val number = (value as? JsonPrimitive)?.takeIf { !it.isString }?.longOrNull ?: throw INVALID_PARAMS
        if (number !in 0..Int.MAX_VALUE) throw INVALID_PARAMS
        return number.toInt()
    }

    private fun string(value: JsonElement?, maximumCodePoints: Int): String? {
        val primitive = value as? JsonPrimitive ?: return null
        if (!primitive.isString) return null
        val text = primitive.contentOrNull ?: return null
        return text.takeIf { it.codePointCount(0, it.length) <= maximumCodePoints }
    }

    private fun requireKeys(value: JsonObject, allowed: Set<String>, fault: Fault) {
        if (!value.keys.all { it in allowed }) throw fault
    }

    private fun requestId(raw: JsonElement): JsonElement? =
        (raw as? JsonObject)?.get("id")?.let { string(it, 128)?.takeIf(String::isNotEmpty)?.let(::JsonPrimitive) }

    private fun listResponse(
        id: JsonPrimitive,
        declarations: List<SemanticDeclaration>,
        maxItems: Int,
        maxBytes: Int,
        cursor: String?,
        byteLimited: Boolean,
    ): JsonObject = success(id, buildJsonObject {
        putJsonObject("catalog") {
            put("id", catalog.identity.id)
            put("generation", catalog.identity.generation)
        }
        putJsonArray("capabilities") {
            declarations.forEach { declaration ->
                add(buildJsonObject {
                    put("id", declaration.id)
                    put("kind", if (declaration.kind == SemanticKind.RESOURCE) "resource" else "action")
                    put("declarationRevision", declaration.declarationRevision)
                })
            }
        }
        putJsonObject("page") {
            put("truncated", cursor != null)
            put("returnedItems", declarations.size)
            putJsonObject("appliedLimits") {
                put("maxItems", maxItems)
                put("maxBytes", maxBytes)
            }
            if (cursor != null) {
                put("nextCursor", cursor)
                val reasons = buildList {
                    if (declarations.size == maxItems) add("maxItems")
                    if (byteLimited) add("maxBytes")
                    if (isEmpty()) add("maxBytes")
                }
                putJsonArray("reasons") { reasons.forEach { add(JsonPrimitive(it)) } }
            }
        }
    })

    private fun declarationValue(declaration: SemanticDeclaration): JsonObject = buildJsonObject {
        put("id", declaration.id)
        put("kind", if (declaration.kind == SemanticKind.RESOURCE) "resource" else "action")
        put("declarationRevision", declaration.declarationRevision)
        when (declaration) {
            is ResourceDeclaration -> {
                declaration.inputSchema?.let { put("inputSchema", schemaValue(it)) }
                put("valueSchema", schemaValue(declaration.valueSchema))
            }
            is ActionDeclaration -> {
                put("inputSchema", schemaValue(declaration.inputSchema))
                putJsonObject("policy") {
                    put("authorization", when (declaration.policy.authorization) {
                        AuthorizationPolicy.NONE -> "none"
                        AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION -> "destructiveAuthorization"
                    })
                    put("retrySafety", when (declaration.policy.retrySafety) {
                        RetrySafety.NO_AUTOMATIC_RETRY -> "noAutomaticRetry"
                        RetrySafety.RETRY_WITH_PROOF_ONLY -> "retryWithProofOnly"
                    })
                }
            }
        }
    }

    private fun schemaValue(handle: SchemaHandle): JsonObject = buildJsonObject {
        put("id", handle.id)
        put("revision", handle.revision)
        put("digest", handle.digest)
    }

    private fun declaredHandles(declaration: SemanticDeclaration): Set<SchemaHandle> = when (declaration) {
        is ResourceDeclaration -> setOfNotNull(declaration.inputSchema, declaration.valueSchema)
        is ActionDeclaration -> setOf(declaration.inputSchema)
    }

    private fun success(id: JsonElement, result: JsonElement): JsonObject = buildJsonObject {
        put("jsonrpc", "2.0")
        put("id", id)
        put("result", result)
    }

    private fun error(id: JsonElement, fault: Fault): ByteArray = encode(buildJsonObject {
        put("jsonrpc", "2.0")
        put("id", id)
        putJsonObject("error") {
            put("code", fault.code)
            put("message", fault.stockMessage)
            putJsonObject("data") {
                put("kind", fault.kind)
                put("retryable", fault.retryable)
            }
        }
    })

    private fun encode(value: JsonElement): ByteArray = JSON.encodeToString(JsonElement.serializer(), value)
        .toByteArray(StandardCharsets.UTF_8)

    private fun strictUtf8(bytes: ByteArray): String = StandardCharsets.UTF_8.newDecoder()
        .onMalformedInput(CodingErrorAction.REPORT)
        .onUnmappableCharacter(CodingErrorAction.REPORT)
        .decode(ByteBuffer.wrap(bytes))
        .toString()

    private fun mapFailure(failure: SemanticFailure): Fault = when (failure.kind) {
        SemanticFailureKind.CAPABILITY_NOT_FOUND -> semanticCapabilityNotFound()
        SemanticFailureKind.SCHEMA_MISMATCH -> semanticSchemaMismatch()
        SemanticFailureKind.UNAVAILABLE -> semanticUnavailable()
        SemanticFailureKind.DISCLOSURE_DENIED -> semanticDisclosureDenied()
        SemanticFailureKind.RESOURCE_EXHAUSTED -> RESOURCE_EXHAUSTED
        else -> INTERNAL_ERROR
    }

    private fun newSessionId(): String = "session_${randomToken()}"
    private fun newCursorToken(): String = "cursor_${randomToken()}"

    private fun randomToken(): String = ByteArray(16).also(random::nextBytes).joinToString("") { "%02x".format(it) }

    private fun semanticCapabilityNotFound() = Fault(-32020, "semantic.capabilityNotFound", "Semantic capability is unavailable", false)
    private fun semanticSchemaMismatch() = Fault(-32021, "semantic.schemaMismatch", "Semantic schema does not match", false)
    private fun semanticUnavailable() = Fault(-32022, "semantic.unavailable", "Semantic capability is unavailable", true)
    private fun semanticDisclosureDenied() = Fault(-32023, "semantic.disclosureDenied", "Semantic disclosure is denied", false)

    private companion object {
        val JSON = Json { ignoreUnknownKeys = false; explicitNulls = false }
        val CAPABILITY_ID = Regex("^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$")
        val SESSION_CAPABILITY = Regex("^[a-z][a-z0-9]*(?:\\.[a-z][a-z0-9]*)+$")
        val METHOD = Regex("^(?!rpc\\.)[a-z][a-z0-9]*(?:\\.[a-z][a-z0-9]*)+$")
        val SCHEMA_ID = Regex("^schema_[A-Za-z0-9._~-]{8,120}$")
        val DIGEST = Regex("^sha256:[a-f0-9]{64}$")
        val SESSION_ID = Regex("^[A-Za-z0-9._~-]{16,128}$")
        const val CORE = "session.core"
        const val SEMANTIC_CATALOG = "semantic.catalog"
        val PARSE_ERROR = Fault(-32700, "parseError", "Parse error", false)
        val INVALID_REQUEST = Fault(-32600, "invalidRequest", "Invalid request", false)
        val METHOD_NOT_FOUND = Fault(-32601, "methodNotFound", "Method not found", false)
        val INVALID_PARAMS = Fault(-32602, "invalidParams", "Invalid params", false)
        val INTERNAL_ERROR = Fault(-32603, "internalError", "Internal error", false)
        val INCOMPATIBLE_PROTOCOL = Fault(-32001, "incompatibleProtocol", "No compatible protocol version", false)
        val SESSION_EXPIRED = Fault(-32002, "sessionExpired", "Session expired", false)
        val CAPABILITY_UNAVAILABLE = Fault(-32003, "capabilityUnavailable", "Capability unavailable", false)
        val RESOURCE_EXHAUSTED = Fault(-32004, "resourceExhausted", "Resource exhausted", false)
        val CURSOR_EXPIRED = Fault(-32006, "cursorExpired", "Cursor expired", false)
    }
}
