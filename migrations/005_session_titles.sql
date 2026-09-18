ALTER TABLE cards RENAME COLUMN description TO task;

UPDATE cards
   SET task = CASE WHEN task = '' THEN title ELSE title || char(10) || char(10) || task END
 WHERE title NOT LIKE '%…';

ALTER TABLE cards DROP COLUMN title;
ALTER TABLE cards ADD COLUMN title TEXT;
