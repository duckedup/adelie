-- ClickBench Q12: top search phrases by row count
SELECT "SearchPhrase", count(*) AS c FROM hits WHERE "SearchPhrase" <> '' GROUP BY "SearchPhrase" ORDER BY c DESC LIMIT 10
