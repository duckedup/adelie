-- ClickBench Q11: two-column GROUP BY, both filtered non-empty
SELECT "MobilePhoneModel", "SearchPhrase", count(*) AS c FROM hits
WHERE "MobilePhoneModel" <> '' AND "SearchPhrase" <> ''
GROUP BY "MobilePhoneModel", "SearchPhrase" ORDER BY c DESC LIMIT 10
