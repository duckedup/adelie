-- OTel 6: count of spans per run.id
SELECT "resource.run.id", count(*) AS c FROM otel.spans GROUP BY "resource.run.id" ORDER BY c DESC
