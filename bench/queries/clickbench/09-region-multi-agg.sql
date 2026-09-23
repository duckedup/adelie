-- ClickBench Q9: multiple aggregates per region, top 10 by row count
SELECT "RegionID", sum("AdvEngineID"), count(*) AS c, avg("ResolutionWidth"), count(DISTINCT "UserID")
FROM hits GROUP BY "RegionID" ORDER BY c DESC LIMIT 10
