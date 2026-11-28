import io.swagger.v3.oas.models.OpenAPI;
import io.swagger.v3.oas.models.media.Schema;
import java.util.ArrayList;
import java.util.Map;
import java.util.Set;
import org.openapitools.codegen.OpenAPINormalizer;

/** utoipa encodes unsigned Rust integers as int32/int64 with a nonnegative minimum. */
public final class FinanceNumbers extends OpenAPINormalizer {
    public FinanceNumbers(OpenAPI api, Map<String, String> rules) {
        super(api, rules);
        flattenMarketEvent(api);
        for (String path : Set.of("/api/v1/options/chain", "/api/v1/options/exposure",
                "/api/v1/options/history", "/api/v1/options/flow/observations",
                "/api/v1/options/flow/verification", "/api/v1/options/flow/snapshot", "/api/v1/market/book",
                "/api/v1/market/subscriptions", "/api/v1/market/subscriptions/coverage")) {
            // Keep the declared schema and let the gateway preserve the original JSON
            // for display reads, including future enum values and added fields.
            var item = api.getPaths().get(path);
            if (item != null) {
                item.getGet().addExtension("x-finance-display-read", true);
            }
        }
        for (String path : Set.of("/api/v1/options/flow/ws", "/api/v1/options/flow/sse")) {
            var item = api.getPaths().get(path);
            if (item != null) {
                // The caller manages the streaming response and upgrade lifecycle.
                item.getGet().addExtension("x-finance-stream-request", true);
            }
        }
    }

    /**
     * F1 intersects a shared envelope and a discriminated payload. The Rust
     * generator otherwise combines all variants into the last payload schema.
     * Keep the typed tagged union as a flattened Serde field instead.
     */
    private static void flattenMarketEvent(OpenAPI api) {
        var schemas = api.getComponents().getSchemas();
        Schema event = schemas.get("Event");
        if (event == null) {
            return;
        }
        var parts = event.getAllOf();
        String messageRef = "#/components/schemas/Message";
        if (parts == null || parts.size() != 3
                || !messageRef.equals(((Schema) parts.get(0)).get$ref())) {
            throw new IllegalArgumentException("Unexpected F1 Event composition");
        }
        Schema envelope = (Schema) parts.get(1);
        var required = new ArrayList<String>(envelope.getRequired());
        required.add("message");
        Schema message = new Schema().$ref(messageRef);
        message.addExtension("x-finance-flatten", true);
        event.setAllOf(null);
        event.setType("object");
        event.setProperties(envelope.getProperties());
        event.addProperty("message", message);
        event.setRequired(required);
    }

    @Override
    public Schema normalizeSchema(Schema schema, Set<Schema> visited) {
        Schema type = super.normalizeSchema(schema, visited);
        if (type == null || type.getMinimum() == null || type.getMinimum().signum() < 0) {
            return type;
        }
        boolean integer = "integer".equals(type.getType())
                || (type.getTypes() != null && type.getTypes().contains("integer"));
        if (integer) {
            String format = type.getFormat();
            if (format == null || "int64".equals(format)) {
                type.setFormat("uint64");
            } else if ("int32".equals(format)) {
                type.setFormat("uint32");
            }
        }
        return type;
    }
}
