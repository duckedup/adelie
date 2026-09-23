-- ClickBench-style: range filter over the date column
SELECT count(*) FROM hits WHERE "EventDate" >= DATE '1970-01-01' AND "EventDate" < DATE '2030-01-01'
