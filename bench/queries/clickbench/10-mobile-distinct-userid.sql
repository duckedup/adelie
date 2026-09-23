-- ClickBench Q10: top mobile models by distinct users, non-empty only
SELECT "MobilePhoneModel", count(DISTINCT "UserID") AS u FROM hits
WHERE "MobilePhoneModel" <> '' GROUP BY "MobilePhoneModel" ORDER BY u DESC LIMIT 10
