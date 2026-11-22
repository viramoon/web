import io.swagger.v3.oas.models.OpenAPI;
import io.swagger.v3.oas.models.media.Schema;
import java.util.Map;
import java.util.Set;
import org.openapitools.codegen.OpenAPINormalizer;

/** utoipa encodes unsigned Rust integers as int32/int64 with a nonnegative minimum. */
public final class FinanceNumbers extends OpenAPINormalizer {
    public FinanceNumbers(OpenAPI api, Map<String, String> rules) {
        super(api, rules);
    }

    @Override
    public Schema normalizeSchema(Schema schema, Set<Schema> visited) {
        Schema type = super.normalizeSchema(schema, visited);
        if (type == null || type.getMinimum() == null || type.getMinimum().signum() < 0) {
            return type;
        }
        boolean integer = "integer".equals(type.getType())
                || (type.getTypes() != null && type.getTypes().contains("integer"));
        if (integer && ("int32".equals(type.getFormat()) || "int64".equals(type.getFormat()))) {
            type.setFormat("u" + type.getFormat());
        }
        return type;
    }
}
