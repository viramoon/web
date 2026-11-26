CREATE TABLE app.market_watchlists (
    id text PRIMARY KEY CHECK (id ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    revision bigint NOT NULL CHECK (revision > 0),
    instrument_ids text[] NOT NULL CHECK (
        cardinality(instrument_ids) <= 64 AND array_position(instrument_ids, NULL) IS NULL
    )
);
