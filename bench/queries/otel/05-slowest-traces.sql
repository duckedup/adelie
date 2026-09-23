-- OTel 5: slowest traces, by total span duration, top 10
SELECT trace_id, sum(duration_ns) AS total_ns FROM otel.spans GROUP BY trace_id ORDER BY total_ns DESC LIMIT 10
