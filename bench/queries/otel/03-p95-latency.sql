-- OTel 3: p95 latency per service
SELECT "service.name", quantile(duration_ns, 0.95) AS p95_ns FROM otel.spans GROUP BY "service.name" ORDER BY p95_ns DESC
