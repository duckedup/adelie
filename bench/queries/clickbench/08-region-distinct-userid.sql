-- ClickBench Q8: top regions by distinct users
SELECT "RegionID", count(DISTINCT "UserID") AS u FROM hits GROUP BY "RegionID" ORDER BY u DESC LIMIT 10
