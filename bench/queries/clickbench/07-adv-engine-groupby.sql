-- ClickBench Q7: GROUP BY with a <> filter, ORDER BY the aggregate
SELECT "AdvEngineID", count(*) AS c FROM hits WHERE "AdvEngineID" <> 0 GROUP BY "AdvEngineID" ORDER BY c DESC
