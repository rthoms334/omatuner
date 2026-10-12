//! The served-playlist guard and playlist views (DESIGN §40 §8.1.4–5): a stable
//! `#EXT-X-TARGETDURATION`, the IDR check on each new segment (two violations → "irregular"),
//! relative names only; views that start at segment k (`MEDIA-SEQUENCE:k`, PDT kept, growing only
//! at the end) for seeks inside what's produced. OWNER: CAST-4.

// CAST-4
