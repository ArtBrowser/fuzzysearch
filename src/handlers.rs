use crate::types::*;
use crate::utils::{extract_e621_rows, extract_fa_rows};
use crate::{rate_limit, Pool};
use log::debug;
use warp::{reject, Rejection, Reply};

fn map_bb8_err(err: bb8::RunError<tokio_postgres::Error>) -> Rejection {
    reject::custom(Error::from(err))
}

fn map_postgres_err(err: tokio_postgres::Error) -> Rejection {
    reject::custom(Error::from(err))
}

#[derive(Debug)]
enum Error {
    BB8(bb8::RunError<tokio_postgres::Error>),
    Postgres(tokio_postgres::Error),
    InvalidData,
    ApiKey,
    RateLimit,
}

impl From<bb8::RunError<tokio_postgres::Error>> for Error {
    fn from(err: bb8::RunError<tokio_postgres::Error>) -> Self {
        Error::BB8(err)
    }
}

impl From<tokio_postgres::Error> for Error {
    fn from(err: tokio_postgres::Error) -> Self {
        Error::Postgres(err)
    }
}

impl warp::reject::Reject for Error {}

pub async fn search_image(
    form: warp::multipart::FormData,
    db: Pool,
    api_key: String,
) -> Result<impl Reply, Rejection> {
    let db = db.get().await.map_err(map_bb8_err)?;

    rate_limit!(&api_key, &db, image_limit, "image");

    use bytes::BufMut;
    use futures_util::StreamExt;
    let parts: Vec<_> = form.collect().await;
    let mut parts = parts
        .into_iter()
        .map(|part| {
            let part = part.unwrap();
            (part.name().to_string(), part)
        })
        .collect::<std::collections::HashMap<_, _>>();
    let image = parts.remove("image").unwrap();

    let bytes = image
        .stream()
        .fold(bytes::BytesMut::new(), |mut b, data| {
            b.put(data.unwrap());
            async move { b }
        })
        .await;

    let hash = {
        let hasher = crate::get_hasher();
        let image = image::load_from_memory(&bytes).unwrap();
        hasher.hash_image(&image)
    };

    let mut buf: [u8; 8] = [0; 8];
    buf.copy_from_slice(&hash.as_bytes());

    let num = i64::from_be_bytes(buf);

    debug!("Matching hash {}", num);

    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&num];

    let fa = db.query(
        "SELECT
                submission.id,
                submission.url,
                submission.filename,
                submission.file_id,
                submission.hash,
                submission.hash_int,
                artist.name
            FROM
                submission
            JOIN artist
                ON artist.id = submission.artist_id
            WHERE
                hash_int <@ ($1, 10)",
        &params,
    );

    let e621 = db.query(
        "SELECT
                e621.id,
                e621.hash,
                e621.data->>'file_url' url,
                e621.data->>'md5' md5,
                sources.list sources,
                artists.list artists,
                (e621.data->>'md5') || '.' || (e621.data->>'file_ext') filename
            FROM
                e621,
                LATERAL (
                    SELECT array_agg(s) list
                    FROM jsonb_array_elements_text(data->'sources') s
                ) sources,
                LATERAL (
                    SELECT array_agg(s) list
                    FROM jsonb_array_elements_text(data->'artist') s
                ) artists
            WHERE
                hash <@ ($1, 10)",
        &params,
    );

    let results = futures::future::join(fa, e621).await;
    let (fa_results, e621_results) = (results.0.unwrap(), results.1.unwrap());

    let mut items = Vec::with_capacity(fa_results.len() + e621_results.len());

    items.extend(extract_fa_rows(fa_results, Some(&hash.as_bytes())));
    items.extend(extract_e621_rows(e621_results, Some(&hash.as_bytes())));

    items.sort_by(|a, b| {
        a.distance
            .unwrap_or(u64::max_value())
            .partial_cmp(&b.distance.unwrap_or(u64::max_value()))
            .unwrap()
    });

    let similarity = ImageSimilarity {
        hash: num,
        matches: items,
    };

    Ok(warp::reply::json(&similarity))
}

pub async fn search_file(
    opts: FileSearchOpts,
    db: Pool,
    api_key: String,
) -> Result<impl Reply, Rejection> {
    let db = db.get().await.map_err(map_bb8_err)?;

    rate_limit!(&api_key, &db, name_limit, "file");

    let (filter, val): (&'static str, &(dyn tokio_postgres::types::ToSql + Sync)) =
        if let Some(ref id) = opts.id {
            ("file_id = $1", id)
        } else if let Some(ref name) = opts.name {
            ("lower(filename) = lower($1)", name)
        } else if let Some(ref url) = opts.url {
            ("lower(url) = lower($1)", url)
        } else {
            return Err(warp::reject::custom(Error::InvalidData));
        };

    debug!("Searching for {:?}", opts);

    let query = format!(
        "SELECT
            submission.id,
            submission.url,
            submission.filename,
            submission.file_id,
            artist.name
        FROM
            submission
        JOIN artist
            ON artist.id = submission.artist_id
        WHERE
            {}
        LIMIT 10",
        filter
    );

    let matches: Vec<_> = db
        .query::<str>(&*query, &[val])
        .await
        .map_err(map_postgres_err)?
        .into_iter()
        .map(|row| File {
            id: row.get("id"),
            url: row.get("url"),
            filename: row.get("filename"),
            artists: row
                .get::<&str, Option<String>>("name")
                .map(|artist| vec![artist]),
            distance: None,
            hash: None,
            site_info: Some(SiteInfo::FurAffinity(FurAffinityFile {
                file_id: row.get("file_id"),
            })),
        })
        .collect();

    Ok(warp::reply::json(&matches))
}

pub async fn handle_rejection(err: Rejection) -> Result<impl Reply, std::convert::Infallible> {
    let (code, message) = if err.is_not_found() {
        (
            warp::http::StatusCode::NOT_FOUND,
            "This page does not exist",
        )
    } else if let Some(err) = err.find::<Error>() {
        match err {
            Error::BB8(_inner) => (
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                "A database error occured",
            ),
            Error::Postgres(_inner) => (
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                "A database error occured",
            ),
            Error::InvalidData => (
                warp::http::StatusCode::BAD_REQUEST,
                "Unable to operate on provided data",
            ),
            Error::ApiKey => (
                warp::http::StatusCode::UNAUTHORIZED,
                "Invalid API key provided",
            ),
            Error::RateLimit => (
                warp::http::StatusCode::TOO_MANY_REQUESTS,
                "Your API token is rate limited",
            ),
        }
    } else {
        (
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            "An unknown error occured",
        )
    };

    let json = warp::reply::json(&ErrorMessage {
        code: code.as_u16(),
        message: message.into(),
    });

    Ok(warp::reply::with_status(json, code))
}
