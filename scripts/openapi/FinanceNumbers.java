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
