-- ClickBench-style: LIKE filter over the search phrase vocabulary
SELECT count(*) FROM hits WHERE "SearchPhrase" LIKE '%rust%'
