-- Which files a reviewer has ticked off, so a re-render keeps them collapsed.
-- Rows are dropped when the card is.
CREATE TABLE review_viewed (
    card_id   INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    file_path TEXT NOT NULL,
    PRIMARY KEY (card_id, file_path)
);
