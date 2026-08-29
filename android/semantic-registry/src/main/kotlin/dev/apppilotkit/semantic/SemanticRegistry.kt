package dev.apppilotkit.semantic

import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.math.BigDecimal
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
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
private val JSON_INTEGER = Regex("^-?[0-9]+$")

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

/** Target-local identity used to bind action policy and authorization checks. */
data class TargetActionContext(
    val targetId: String,
    val processGeneration: Long,
    val sessionId: String,
) {
    init {
        require(targetId.isNotBlank())
        require(processGeneration >= 1)
        require(sessionId.isNotBlank())
    }
}

/** A resolved policy. There is intentionally no implicit or permissive policy. */
data class EffectiveActionPolicy(
    val authorization: AuthorizationPolicy,
    val retrySafety: RetrySafety,
)

/** The policy identity projected from a Semantic Action or an ordinary mutation. */
data class ActionPolicySubject(
    val id: String,
    val authorization: AuthorizationPolicy,
    val retrySafety: RetrySafety,
)

/** The exact canonical material to which a destructive grant is bound. */
data class CanonicalActionBinding(
    val targetId: String,
    val processGeneration: Long,
    val sessionId: String,
    val capability: String,
    val declarationRevision: Int,
    val inputSchema: SchemaHandle,
    val inputDigest: String,
)

data class DestructiveAuthorizationRequest(
    val binding: CanonicalActionBinding,
    val grant: String,
)

fun interface EffectiveActionPolicyResolver {
    fun resolve(context: TargetActionContext, subject: ActionPolicySubject): EffectiveActionPolicy?
}

interface DestructiveAuthorizationValidator {
    /** Read-only validation. It must not consume or mutate the grant. */
    fun validate(request: DestructiveAuthorizationRequest): Boolean

    /** Atomically revalidates every binding and expiry, then consumes at most once. */
    fun consume(request: DestructiveAuthorizationRequest): Boolean
}

/**
 * Target-owned evidence lifecycle. Production composition must inject a real implementation;
 * there is deliberately no no-op default.
 */
interface ActionEvidencePort {
    fun captureBefore(context: TargetActionContext)
    fun observeStability(context: TargetActionContext)
    fun captureAfter(context: TargetActionContext)
}

data class TargetActionRequest(
    val invocation: ActionInvocation,
    val context: TargetActionContext,
    val authorizationGrant: String?,
    val sessionIsActive: () -> Boolean,
)

enum class TargetActionResult { COMPLETED }

enum class TargetActionFailureKind {
    SESSION_EXPIRED,
    POLICY_DENIED,
    CONFLICT,
    PRE_DISPATCH_FAILED,
    OUTCOME_UNKNOWN,
}

class TargetActionFailure(val kind: TargetActionFailureKind) : RuntimeException(
    when (kind) {
        TargetActionFailureKind.SESSION_EXPIRED -> "Session expired."
        TargetActionFailureKind.POLICY_DENIED -> "Action policy is denied."
        TargetActionFailureKind.CONFLICT -> "Action conflicts with an in-flight mutation."
        TargetActionFailureKind.PRE_DISPATCH_FAILED -> "Action failed before dispatch."
        TargetActionFailureKind.OUTCOME_UNKNOWN -> "Action outcome is unknown."
    },
)

/** The sole public mutation facade; it never exposes a prepared handler. */
interface TargetActionCoordinator {
    fun invoke(request: TargetActionRequest): TargetActionResult
}

/** Internal provider seam for non-semantic mutations; protocol runtimes never receive it. */
internal data class OrdinaryMutationRequest(
    val context: TargetActionContext,
    val subject: ActionPolicySubject,
    val authorizationGrant: String?,
    val sessionIsActive: () -> Boolean,
    val mutation: () -> Unit,
)

internal interface TargetActionMutationCoordinator : TargetActionCoordinator {
    fun invokeOrdinary(request: OrdinaryMutationRequest): TargetActionResult
}

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

    /**
     * Creates the only supported Action execution facade for this frozen Catalog.
     * The actual preparation and dispatch closure remain private to this registry.
     */
    fun targetActionCoordinator(
        targetId: String,
        policyResolver: EffectiveActionPolicyResolver,
        destructiveAuthorizationValidator: DestructiveAuthorizationValidator,
        evidence: ActionEvidencePort,
    ): TargetActionCoordinator = targetActionMutationCoordinator(
        targetId,
        policyResolver,
        destructiveAuthorizationValidator,
        evidence,
    )

    internal fun targetActionMutationCoordinator(
        targetId: String,
        policyResolver: EffectiveActionPolicyResolver,
        destructiveAuthorizationValidator: DestructiveAuthorizationValidator,
        evidence: ActionEvidencePort,
    ): TargetActionMutationCoordinator = Coordinator(
        targetId,
        policyResolver,
        destructiveAuthorizationValidator,
        evidence,
    )

    private fun validateAction(request: ActionInvocation, context: TargetActionContext): ValidatedAction {
        val entry = matchingEntry(request.capability, request.declarationRevision) as? ActionEntry
            ?: throw SemanticFailure(SemanticFailureKind.CAPABILITY_NOT_FOUND)
        if (request.inputSchema != entry.declaration.inputSchema) {
            throw SemanticFailure(SemanticFailureKind.SCHEMA_MISMATCH)
        }
        val typedInput = safeDecode(entry.inputCodec, request.input)
        if (!safeAvailability(entry.available)) {
            throw SemanticFailure(SemanticFailureKind.UNAVAILABLE)
        }
        return ValidatedAction(
            declaration = entry.declaration,
            binding = CanonicalActionBinding(
                targetId = context.targetId,
                processGeneration = context.processGeneration,
                sessionId = context.sessionId,
                capability = entry.declaration.id,
                declarationRevision = entry.declaration.declarationRevision,
                inputSchema = entry.declaration.inputSchema,
                inputDigest = canonicalDigest(request.input),
            ),
            typedInput = typedInput,
            invoke = entry.invoke,
        )
    }

    private inner class Coordinator(
        private val targetId: String,
        private val policyResolver: EffectiveActionPolicyResolver,
        private val destructiveAuthorizationValidator: DestructiveAuthorizationValidator,
        private val evidence: ActionEvidencePort,
    ) : TargetActionMutationCoordinator {
        init {
            require(targetId.isNotBlank())
        }

        override fun invoke(request: TargetActionRequest): TargetActionResult {
            ensureTargetContext(request.context)
            ensureSessionActive(request.sessionIsActive)
            val validated = validateAction(request.invocation, request.context)
            val subject = actionSubject(validated.declaration)
            val effective = resolveEffectivePolicy(request.context, subject)
            val authorization = destructiveAuthorization(
                subject = subject,
                binding = validated.binding,
                grant = request.authorizationGrant,
            )
            if (authorization != null && !validateAuthorization(authorization)) {
                throw TargetActionFailure(TargetActionFailureKind.POLICY_DENIED)
            }
            if (!TargetWriters.tryAcquire(targetId)) {
                throw TargetActionFailure(TargetActionFailureKind.CONFLICT)
            }
            try {
                return performValidatedMutation(
                    context = request.context,
                    sessionIsActive = request.sessionIsActive,
                    authorization = authorization,
                ) {
                    try {
                        validated.invoke(validated.typedInput)
                    } catch (_: Throwable) {
                        throw TargetActionFailure(TargetActionFailureKind.OUTCOME_UNKNOWN)
                    }
                }
            } finally {
                TargetWriters.release(targetId)
            }
        }

        override fun invokeOrdinary(request: OrdinaryMutationRequest): TargetActionResult {
            ensureTargetContext(request.context)
            ensureSessionActive(request.sessionIsActive)
            val effective = resolveEffectivePolicy(request.context, request.subject)
            val binding = ordinaryBinding(request.context, request.subject)
            val authorization = destructiveAuthorization(
                subject = request.subject,
                binding = binding,
                grant = request.authorizationGrant,
            )
            if (authorization != null && !validateAuthorization(authorization)) {
                throw TargetActionFailure(TargetActionFailureKind.POLICY_DENIED)
            }
            if (!TargetWriters.tryAcquire(targetId)) {
                throw TargetActionFailure(TargetActionFailureKind.CONFLICT)
            }
            try {
                return performValidatedMutation(
                    context = request.context,
                    sessionIsActive = request.sessionIsActive,
                    authorization = authorization,
                ) {
                    try {
                        request.mutation()
                    } catch (_: Throwable) {
                        throw TargetActionFailure(TargetActionFailureKind.OUTCOME_UNKNOWN)
                    }
                }
            } finally {
                TargetWriters.release(targetId)
            }
        }

        private fun performValidatedMutation(
            context: TargetActionContext,
            sessionIsActive: () -> Boolean,
            authorization: DestructiveAuthorizationRequest?,
            dispatch: () -> Unit,
        ): TargetActionResult {
            ensureSessionActive(sessionIsActive)
            if (authorization != null && !consumeAuthorization(authorization)) {
                throw TargetActionFailure(TargetActionFailureKind.POLICY_DENIED)
            }
            try {
                evidence.captureBefore(context)
            } catch (_: Throwable) {
                throw TargetActionFailure(TargetActionFailureKind.PRE_DISPATCH_FAILED)
            }
            ensureSessionActive(sessionIsActive)

            claimDispatch(dispatch, DispatchAuthority()).handoff()
            ensureSessionActiveAfterHandoff(sessionIsActive)
            try {
                evidence.observeStability(context)
            } catch (_: Throwable) {
                throw TargetActionFailure(TargetActionFailureKind.OUTCOME_UNKNOWN)
            }
            ensureSessionActiveAfterHandoff(sessionIsActive)
            try {
                evidence.captureAfter(context)
            } catch (_: Throwable) {
                throw TargetActionFailure(TargetActionFailureKind.OUTCOME_UNKNOWN)
            }
            ensureSessionActiveAfterHandoff(sessionIsActive)
            return TargetActionResult.COMPLETED
        }

        private fun resolveEffectivePolicy(
            context: TargetActionContext,
            subject: ActionPolicySubject,
        ): EffectiveActionPolicy {
            val effective = try {
                policyResolver.resolve(context, subject)
            } catch (_: Throwable) {
                null
            } ?: throw TargetActionFailure(TargetActionFailureKind.POLICY_DENIED)
            if (effective.authorization != subject.authorization ||
                effective.retrySafety != subject.retrySafety
            ) {
                throw TargetActionFailure(TargetActionFailureKind.POLICY_DENIED)
            }
            return effective
        }

        private fun destructiveAuthorization(
            subject: ActionPolicySubject,
            binding: CanonicalActionBinding,
            grant: String?,
        ): DestructiveAuthorizationRequest? {
            if (subject.authorization != AuthorizationPolicy.DESTRUCTIVE_AUTHORIZATION) return null
            if (grant.isNullOrEmpty()) {
                throw TargetActionFailure(TargetActionFailureKind.POLICY_DENIED)
            }
            return DestructiveAuthorizationRequest(
                binding = binding,
                grant = grant,
            )
        }

        private fun validateAuthorization(request: DestructiveAuthorizationRequest): Boolean = try {
            destructiveAuthorizationValidator.validate(request)
        } catch (_: Throwable) {
            false
        }

        private fun consumeAuthorization(request: DestructiveAuthorizationRequest): Boolean = try {
            destructiveAuthorizationValidator.consume(request)
        } catch (failure: TargetActionFailure) {
            throw failure
        } catch (_: Throwable) {
            throw TargetActionFailure(TargetActionFailureKind.PRE_DISPATCH_FAILED)
        }

        private fun claimDispatch(dispatch: () -> Unit, authority: DispatchAuthority): DispatchClaim =
            DispatchClaim(dispatch, authority)

        private fun actionSubject(declaration: ActionDeclaration) = ActionPolicySubject(
            id = declaration.id,
            authorization = declaration.policy.authorization,
            retrySafety = declaration.policy.retrySafety,
        )

        private fun ordinaryBinding(
            context: TargetActionContext,
            subject: ActionPolicySubject,
        ) = CanonicalActionBinding(
            targetId = context.targetId,
            processGeneration = context.processGeneration,
            sessionId = context.sessionId,
            capability = subject.id,
            declarationRevision = ORDINARY_MUTATION_REVISION,
            inputSchema = ORDINARY_MUTATION_SCHEMA,
            inputDigest = ORDINARY_MUTATION_DIGEST,
        )

        private fun ensureTargetContext(context: TargetActionContext) {
            if (context.targetId != targetId) {
                throw TargetActionFailure(TargetActionFailureKind.SESSION_EXPIRED)
            }
        }

        private fun ensureSessionActive(sessionIsActive: () -> Boolean) {
            if (!sessionIsActive()) {
                throw TargetActionFailure(TargetActionFailureKind.SESSION_EXPIRED)
            }
        }

        private fun ensureSessionActiveAfterHandoff(sessionIsActive: () -> Boolean) {
            if (!sessionIsActive()) {
                throw TargetActionFailure(TargetActionFailureKind.OUTCOME_UNKNOWN)
            }
        }
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

private class ValidatedAction(
    val declaration: ActionDeclaration,
    val binding: CanonicalActionBinding,
    val typedInput: Any,
    val invoke: (Any) -> Unit,
)

private class DispatchAuthority

private class DispatchClaim(
    private val dispatch: () -> Unit,
    @Suppress("unused") private val authority: DispatchAuthority,
) {
    private val handedOff = AtomicBoolean(false)

    fun handoff() {
        if (!handedOff.compareAndSet(false, true)) {
            throw TargetActionFailure(TargetActionFailureKind.OUTCOME_UNKNOWN)
        }
        dispatch()
    }
}

private val ORDINARY_MUTATION_REVISION = 1
private val ORDINARY_MUTATION_DIGEST = canonicalDigest(JsonObject(emptyMap()))
private val ORDINARY_MUTATION_SCHEMA = SchemaHandle(
    id = "schema_ordinary_mutation",
    revision = 1,
    digest = ORDINARY_MUTATION_DIGEST,
)

private fun canonicalDigest(value: JsonElement): String = MessageDigest.getInstance("SHA-256")
    .digest(wireJson(canonicalize(value)).toByteArray(StandardCharsets.UTF_8))
    .joinToString(separator = "", prefix = "sha256:") { "%02x".format(it) }

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
        else -> Json.parseToJsonElement(canonicalNumber(value.content))
    }
}

private fun wireJson(value: JsonElement): String = when (value) {
    JsonNull -> "null"
    is JsonObject -> value.entries.joinToString(separator = ",", prefix = "{", postfix = "}") { (key, child) ->
        val encodedKey = Json.encodeToString(JsonPrimitive.serializer(), JsonPrimitive(key))
        "$encodedKey:${wireJson(child)}"
    }
    is JsonArray -> value.joinToString(separator = ",", prefix = "[", postfix = "]") { wireJson(it) }
    is JsonPrimitive -> when {
        value.isString -> Json.encodeToString(JsonPrimitive.serializer(), value)
        value.booleanOrNull != null -> value.content
        else -> canonicalNumber(value.content)
    }
}

private fun canonicalNumber(raw: String): String {
    if (JSON_INTEGER.matches(raw)) {
        val integer = BigDecimal(raw).stripTrailingZeros()
        if (integer.signum() == 0) return "0"
        return integer.toPlainString()
    }
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

    return scientificNumber(decimal)
}

private fun scientificNumber(decimal: BigDecimal): String {
    val digits = decimal.unscaledValue().abs().toString()
    val exponent = digits.length - decimal.scale() - 1
    val signPrefix = if (decimal.signum() < 0) "-" else ""
    val mantissa = if (digits.length == 1) digits else "${digits[0]}.${digits.drop(1)}"
    val exponentSign = if (exponent >= 0) "+" else ""
    return "$signPrefix${mantissa}e$exponentSign$exponent"
}

private object TargetWriters {
    private val busy = ConcurrentHashMap<String, AtomicBoolean>()

    fun tryAcquire(targetId: String): Boolean =
        busy.computeIfAbsent(targetId) { AtomicBoolean(false) }.compareAndSet(false, true)

    fun release(targetId: String) {
        busy[targetId]?.set(false)
    }
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
