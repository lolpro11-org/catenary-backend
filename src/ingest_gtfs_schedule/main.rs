use bb8::PooledConnection;
use geo::CoordsIter;
use gtfs_structures::LocationType;
use gtfs_structures::Route;
use gtfs_structures::Trip;
use itertools::Itertools;
use serde::Serialize;
use serde_json::Error as SerdeError;
use tokio_postgres::Client;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use titlecase::titlecase;
use tokio_postgres::Statement;
mod dmfr;
use bb8_postgres::PostgresConnectionManager;
use futures;
use geo_postgis::ToPostgis;
use gtfs_structures::ContinuousPickupDropOff;
use gtfs_structures::RouteType;
use ordered_float::OrderedFloat;
use postgis::ewkb;
use rayon::prelude::*;
use rgb::RGB;
use std::collections::HashSet;
use std::error::Error;
use std::ops::Deref;
use std::sync::Arc;
use tokio_postgres::NoTls;
extern crate tokio_threadpool;
use tokio::runtime;
extern crate fs_extra;
use fs_extra::dir::get_size;

mod colour_correction;
mod convex_hull;

mod fix_broken_lines;
mod make_prod_index;
mod shape_functions;
struct RealtimeOverride {
    realtimeid: String,
    operatorid: String,
}

pub fn path_exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

pub fn toi64(input: &Option<u32>) -> Option<i64> {
    match input {
        Some(i) => Some(*i as i64),
        None => None,
    }
}

/*struct StopTimePostgres {
    feed_id: String,
    trip_id: String,
    stop_id: String,
    stop_sequence: i32,
    arrival_time: Option<i64>,
    departure_time: Option<i64>,
    stop_headsign: Option<String>,
    point: ewkb::Point
}*/

pub fn location_type_conversion(input: &LocationType) -> i16 {
    match input {
        LocationType::StopPoint => 0,
        LocationType::StopArea => 1,
        LocationType::StationEntrance => 2,
        LocationType::GenericNode => 3,
        LocationType::BoardingArea => 4,
        LocationType::Unknown(i) => *i,
    }
}

pub fn route_type_to_int(input: &RouteType) -> i16 {
    match input {
        RouteType::Tramway => 0,
        RouteType::Subway => 1,
        RouteType::Rail => 2,
        RouteType::Bus => 3,
        RouteType::Ferry => 4,
        RouteType::CableCar => 5,
        RouteType::Gondola => 6,
        RouteType::Funicular => 7,
        RouteType::Coach => 200,
        RouteType::Air => 1100,
        RouteType::Taxi => 1500,
        RouteType::Other(i) => *i,
    }
}

pub fn is_uppercase(string: &str) -> bool {
    string.chars().all(char::is_uppercase)
}

pub fn titlecase_process_new_nooption(input: &String) -> String {
    let mut string = input.to_owned();
    if string.len() >= 7
        && string
            .as_str()
            .chars()
            .all(|s| s.is_ascii_punctuation() || s.is_ascii())
            == true
    {
        //i don't want to accidently screw up Greek, Cryllic, Chinese, Japanese, or other writing systems
        string = titlecase(string.as_str());
    }
    string
}

pub fn titlecase_process_new(input: Option<&String>) -> Option<String> {
    match input {
        Some(s) => Some(titlecase_process_new_nooption(s)),
        None => None,
    }
}

pub fn make_hashmap_stops_to_route_types_and_ids(
    gtfs: &gtfs_structures::Gtfs,
) -> (HashMap<String, Vec<i16>>, HashMap<String, Vec<String>>) {
    let mut stop_to_route_types: HashMap<String, Vec<i16>> = HashMap::new();
    let mut stop_to_route_ids: HashMap<String, Vec<String>> = HashMap::new();

    for (trip_id, trip) in &gtfs.trips {
        for stoptime in &trip.stop_times {
            match gtfs.get_route(&trip.route_id) {
                Ok(route) => {
                    let route_type_num = route_type_to_int(&route.route_type);

                    stop_to_route_types
                        .entry(stoptime.stop.id.to_owned())
                        .and_modify(|types| {
                            if !types.contains(&route_type_num) {
                                types.push(route_type_num);

                                types.dedup();
                            }
                        })
                        .or_insert(vec![route_type_num]);

                    stop_to_route_ids
                        .entry(stoptime.stop.id.to_owned())
                        .and_modify(|types| {
                            if !types.contains(&route.id) {
                                types.push(route.id.to_owned());

                                types.dedup();
                            }
                        })
                        .or_insert(vec![route.id.to_owned()]);
                }
                _ => {}
            }
        }
    }
    (stop_to_route_types, stop_to_route_ids)
}

//returns (stop_id_to_children_ids, stop_ids_to_children_route_types)
pub fn make_hashmaps_of_children_stop_info(
    gtfs: &gtfs_structures::Gtfs,
    stop_to_route_types: &HashMap<String, Vec<i16>>,
    stop_to_route_ids: &HashMap<String, Vec<String>>,
) -> (HashMap<String, Vec<String>>, HashMap<String, Vec<i16>>) {
    let mut stop_id_to_children_ids: HashMap<String, Vec<String>> = HashMap::new();
    let mut stop_ids_to_children_route_types: HashMap<String, Vec<i16>> = HashMap::new();

    for (stop_id, stop) in &gtfs.stops {
        if stop.parent_station.is_some() {
            stop_id_to_children_ids
                .entry(stop.parent_station.as_ref().unwrap().to_owned())
                .and_modify(|children_ids| {
                    if !children_ids.contains(&stop_id) {
                        children_ids.push(stop_id.to_owned())
                    }
                })
                .or_insert(vec![stop_id.to_owned()]);

            let route_types_for_this_stop = stop_to_route_types.get(stop_id);

            if route_types_for_this_stop.is_some() {
                stop_ids_to_children_route_types
                    .entry(stop.parent_station.as_ref().unwrap().to_owned())
                    .and_modify(|children_route_types| {
                        children_route_types.extend(route_types_for_this_stop.unwrap());

                        children_route_types.dedup();
                    })
                    .or_insert(route_types_for_this_stop.unwrap().to_owned());
            }
        }
    }

    (stop_id_to_children_ids, stop_ids_to_children_route_types)
}

async fn makedb(client: &Client, schemaname: String, is_prod: Option<bool>, startfresh: Option<bool>) {
    client
        .batch_execute(
            "
        CREATE EXTENSION IF NOT EXISTS postgis;
        CREATE EXTENSION IF NOT EXISTS hstore;
            ",
        )
        .await
        .unwrap();

    if startfresh.unwrap_or(false) {
        client
            .batch_execute(format!("DROP SCHEMA IF EXISTS {} CASCADE;", schemaname).as_str())
            .await
            .unwrap();
    }

    client
        .batch_execute(
            format!(
                "
        CREATE SCHEMA IF NOT EXISTS {schemaname};"
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {schemaname}.gtfs_errors (
            onestop_feed_id text PRIMARY KEY,
            error text
        )"
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {schemaname}.feeds_updated (
            onestop_feed_id text PRIMARY KEY,
            created_trips boolean,
            updated_trips_time_ms bigint
        );"
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.static_feeds (
            onestop_feed_id text PRIMARY KEY,
            only_realtime_ref text,
            operators text[],
            operators_to_gtfs_ids hstore,
            realtime_onestop_ids text[],
            realtime_onestop_ids_to_gtfs_ids hstore,
            max_lat double precision NOT NULL,
            max_lon double precision NOT NULL,
            min_lat double precision NOT NULL,
            min_lon double precision NOT NULL,
            hull GEOMETRY(POLYGON,4326) NOT NULL
        );",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.operators (
            onestop_operator_id text PRIMARY KEY,
            name text,
            gtfs_static_feeds text[],
            gtfs_realtime_feeds text[],
            static_onestop_feeds_to_gtfs_ids hstore,
            realtime_onestop_feeds_to_gtfs_ids hstore
        );",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "CREATE TABLE IF NOT EXISTS {}.realtime_feeds (
                    onestop_feed_id text PRIMARY KEY,
                    name text,
                    operators text[],
                    operators_to_gtfs_ids hstore,
                    max_lat double precision,
                    max_lon double precision,
                    min_lat double precision,
                    min_lon double precision
                );",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.stops (
            onestop_feed_id text NOT NULL,
            gtfs_id text NOT NULL,
            name text NOT NULL,
            displayname text NOT NULL,
            code text,
            gtfs_desc text,
            location_type smallint,
            parent_station text,
            zone_id text,
            url text,
            point GEOMETRY(POINT,4326) NOT NULL,
            timezone text,
            wheelchair_boarding int,
            primary_route_type text,
            level_id text,
            platform_code text,
            routes text[],
            route_types smallint[],
            children_ids text[],
            children_route_types smallint[],
            station_feature boolean,
            hidden boolean,
            location_alias text[],
            PRIMARY KEY (onestop_feed_id, gtfs_id)
        )",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.stoptimes (
            onestop_feed_id text NOT NULL,
            trip_id text NOT NULL,
            stop_sequence int NOT NULL,
            arrival_time bigint,
            departure_time bigint,
            stop_id text NOT NULL,
            stop_headsign text,
            pickup_type int,
            drop_off_type int,
            shape_dist_traveled double precision,
            timepoint int,
            continuous_pickup smallint,
            continuous_drop_off smallint,
            point GEOMETRY(POINT,4326) NOT NULL,
            route_id text,
            PRIMARY KEY (onestop_feed_id, trip_id, stop_sequence)
        )",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.routes (
            route_id text NOT NULL,
            onestop_feed_id text NOT NULL,
            short_name text NOT NULL,
            long_name text NOT NULL,
            gtfs_desc text,
            route_type smallint NOT NULL,
            url text,
            agency_id text,
            gtfs_order int,
            color text,
            text_color text,
            continuous_pickup smallint,
            continuous_drop_off smallint,
            shapes_list text[],
            PRIMARY KEY (onestop_feed_id, route_id)
        );",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.shapes (
            onestop_feed_id text NOT NULL,
            shape_id text NOT NULL,
            linestring GEOMETRY(LINESTRING,4326) NOT NULL,
            color text,
            routes text[],
            route_type smallint NOT NULL,
            route_label text,
            text_color text,
            PRIMARY KEY (onestop_feed_id,shape_id)
        );",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    client
        .batch_execute(
            format!(
                "
        CREATE TABLE IF NOT EXISTS {}.trips (
            trip_id text NOT NULL,
            onestop_feed_id text NOT NULL,
            route_id text NOT NULL,
            service_id text NOT NULL,
            trip_headsign text,
            has_stop_headsign boolean,
            stop_headsigns text[],
            trip_short_name text,
            direction_id int,
            block_id text,
            shape_id text,
            wheelchair_accessible int,
            bikes_allowed int,
            PRIMARY KEY (onestop_feed_id, trip_id)
        );",
                schemaname
            )
            .as_str(),
        )
        .await
        .unwrap();

    if is_prod.unwrap_or(false) {
        println!("making martin functions");
        make_prod_index::make_prod_index(&client, &schemaname.to_string()).await;
        shape_functions::render_vector_tile_functions(&client, &schemaname.to_string()).await;
    }

    println!("Finished making database");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let postgresstring = arguments::parse(std::env::args())
        .expect("Add a postgres string via --postgres <string>")
        .get::<String>("postgres");

    let threads = arguments::parse(std::env::args())
        .expect("Add a thread count via --threads <positive int>")
        .get::<usize>("threads");

    let threadcount = threads.unwrap();

    let postgresstring = match postgresstring {
        Some(s) => s,
        None => {
            panic!("Add a postgres string via --postgres <string>");
        }
    };

    let startfresh = arguments::parse(std::env::args())
        .unwrap()
        .get::<bool>("startfresh");

    let limittostaticfeed = arguments::parse(std::env::args())
        .unwrap()
        .get::<String>("limittostaticfeed");

    let is_prod = arguments::parse(std::env::args())
        .unwrap()
        .get::<bool>("isprod");

    let skiptrips = arguments::parse(std::env::args())
        .unwrap()
        .get::<bool>("skiptrips")
        .unwrap_or_else(|| false);

    let soft_insert = arguments::parse(std::env::args())
        .unwrap()
        .get::<bool>("softinsert");
    
    let force_wipe = arguments::parse(std::env::args())
        .unwrap()
        .get::<bool>("forcewipe")
        .unwrap_or_else(|| false);

    if (startfresh.unwrap_or(false) && is_prod.unwrap_or(false) && force_wipe) {
        panic!("Cannot wipe the prod server without --forcewipe true");
    }

    let schemaname = match is_prod {
        Some(s) => {
            if s {
                "gtfs"
            } else {
                "gtfs_stage"
            }
        }
        None => "gtfs_stage",
    };

    // Connect to the database.
    let (client, connection) = tokio_postgres::connect(&postgresstring, NoTls).await?;

    // The connection object performs the actual communication with the database,
    // so spawn it off to run on its own.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    makedb(&client, schemaname.to_string(), is_prod, startfresh).await;
    #[derive(Debug, Clone)]
    struct OperatorPairInfo {
        operator_id: String,
        gtfs_agency_id: Option<String>,
    }

    if fs::read_dir("transitland-atlas/feeds").is_err() {
        println!("Could not read that directory!");
        return Err(Box::<dyn std::error::Error>::from(
            "Could not read that directory!",
        ));
    }

    let entries = fs::read_dir("transitland-atlas/feeds").unwrap();
    let mut feedhashmap: BTreeMap<String, dmfr::Feed> = BTreeMap::new();
    let mut operatorhashmap: BTreeMap<String, dmfr::Operator> = BTreeMap::new();
    let mut operator_to_feed_hashmap: BTreeMap<String, Vec<dmfr::OperatorAssociatedFeedsItem>> = BTreeMap::new();
    let mut feed_to_operator_hashmap: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut feed_to_operator_pairs_hashmap: BTreeMap<String, Vec<OperatorPairInfo>> = BTreeMap::new();
    let feeds_to_discard = vec![
        "f-9q8y-sfmta",
        "f-9qc-westcat~ca~us",
        "f-9q9-actransit",
        "f-9q9-vta",
        "f-9q8yy-missionbaytma~ca~us",
        "f-9qbb-marintransit",
        "f-9q8-samtrans",
        "f-9q9-bart",
        "f-9q9-caltrain",
        "f-9qc3-riovistadeltabreeze",
    ];
    for entry in entries {
        if let Ok(entry) = entry {
            if let Some(file_name) = entry.file_name().to_str() {
                println!("{}", file_name);
                let contents = fs::read_to_string(format!("transitland-atlas/feeds/{}", file_name));
                if contents.is_err() {
                    eprintln!("Error Reading File: {}", contents.unwrap_err());
                    continue;
                }
                let dmfrinfo: Result<dmfr::DistributedMobilityFeedRegistry, SerdeError> =
                    serde_json::from_str(&contents.unwrap());
                match dmfrinfo {
                    Ok(dmfrinfo) => {
                        dmfrinfo.feeds.iter().for_each(|feed| {
                            for eachoperator in feed.operators.to_owned().into_iter() {
                                if feed_to_operator_pairs_hashmap.contains_key(&feed.id) {
                                    let mut existing_operator_pairs =
                                        feed_to_operator_pairs_hashmap
                                            .get(&feed.id)
                                            .unwrap()
                                            .to_owned();
                                    existing_operator_pairs.push(OperatorPairInfo {
                                        operator_id: eachoperator.onestop_id.to_owned(),
                                        gtfs_agency_id: None,
                                    });
                                    feed_to_operator_pairs_hashmap
                                        .insert(feed.id.to_owned(), existing_operator_pairs);
                                } else {
                                    feed_to_operator_pairs_hashmap.insert(
                                        feed.id.to_owned(),
                                        vec![OperatorPairInfo {
                                            operator_id: eachoperator.onestop_id.to_owned(),
                                            gtfs_agency_id: None,
                                        }],
                                    );
                                }

                                feed_to_operator_hashmap
                                    .entry(feed.id.to_owned())
                                    .and_modify(|value| {
                                        value.push(eachoperator.onestop_id.to_owned())
                                    })
                                    .or_insert(vec![eachoperator.onestop_id.to_owned()]);

                                operator_to_feed_hashmap
                                    .entry(eachoperator.onestop_id)
                                    .or_insert(vec![dmfr::OperatorAssociatedFeedsItem {
                                        feed_onestop_id: Some(feed.id.to_owned()),
                                        gtfs_agency_id: None,
                                    }]);
                            }
                            //println!("Feed {}: {:#?}", feed.id.to_owned(), feed);
                            feedhashmap
                                .entry(feed.id.to_owned())
                                .or_insert(feed.to_owned());

                            feed.operators.iter().for_each(|operator| {
                                operatorhashmap
                                    .insert(operator.onestop_id.to_owned(), operator.to_owned());
                                if operator_to_feed_hashmap.contains_key(&operator.onestop_id) {
                                    //combine the feeds for this operator together
                                    let mut existing_associated_feeds = operator_to_feed_hashmap
                                        .get(&operator.onestop_id)
                                        .unwrap()
                                        .to_owned();
                                    let existing_feed_ids = operator_to_feed_hashmap
                                        .get(&operator.onestop_id)
                                        .unwrap()
                                        .par_iter()
                                        .map(|associated_feed| {
                                            associated_feed.feed_onestop_id.to_owned().unwrap()
                                        })
                                        .collect::<Vec<String>>();
                                    operator
                                        .associated_feeds
                                        .iter()
                                        .for_each(|associated_feed| {
                                            if !existing_feed_ids.contains(
                                                &associated_feed
                                                    .feed_onestop_id
                                                    .to_owned()
                                                    .unwrap_or_else(|| feed.id.to_owned()),
                                            ) {
                                                existing_associated_feeds
                                                    .push(associated_feed.to_owned());
                                            }
                                        });
                                    operator_to_feed_hashmap.insert(
                                        operator.onestop_id.to_owned(),
                                        existing_associated_feeds,
                                    );
                                } else {
                                    operator_to_feed_hashmap.insert(
                                        operator.onestop_id.to_owned(),
                                        operator.associated_feeds.to_owned(),
                                    );
                                }
                            });
                        });
                        dmfrinfo.operators.iter().for_each(|operator| {
                            operatorhashmap
                                .insert(operator.onestop_id.to_owned(), operator.to_owned());
                            for feed in operator.associated_feeds.iter() {
                                if feed.feed_onestop_id.is_some() {
                                    feed_to_operator_pairs_hashmap
                                        .entry(feed.feed_onestop_id.as_ref().unwrap().to_owned())
                                        .and_modify(|existing_operator_pairs| {
                                            existing_operator_pairs.push(OperatorPairInfo {
                                                operator_id: operator.onestop_id.to_owned(),
                                                gtfs_agency_id: feed.gtfs_agency_id.to_owned(),
                                            });
                                        })
                                        .or_insert(vec![OperatorPairInfo {
                                            operator_id: operator.onestop_id.to_owned(),
                                            gtfs_agency_id: feed.gtfs_agency_id.to_owned(),
                                        }]);
                                }
                            }
                            if operator_to_feed_hashmap.contains_key(&operator.onestop_id) {
                                //combine the feeds for this operator together
                                let mut existing_associated_feeds = operator_to_feed_hashmap
                                    .get(&operator.onestop_id)
                                    .unwrap()
                                    .to_owned();
                                let existing_feed_ids = operator_to_feed_hashmap
                                    .get(&operator.onestop_id)
                                    .unwrap()
                                    .iter()
                                    .filter(|associated_feed| {
                                        associated_feed.feed_onestop_id.is_some()
                                    })
                                    .map(|associated_feed| {
                                        associated_feed.feed_onestop_id.to_owned().unwrap()
                                    })
                                    .collect::<Vec<String>>();
                                operator
                                    .associated_feeds
                                    .iter()
                                    .for_each(|associated_feed| {
                                        if !existing_feed_ids.contains(
                                            &associated_feed.feed_onestop_id.to_owned().unwrap(),
                                        ) {
                                            existing_associated_feeds
                                                .push(associated_feed.to_owned());
                                        }
                                    });
                                operator_to_feed_hashmap.insert(
                                    operator.onestop_id.to_owned(),
                                    existing_associated_feeds,
                                );
                            } else {
                                operator_to_feed_hashmap.insert(
                                    operator.onestop_id.to_owned(),
                                    operator.associated_feeds.to_owned(),
                                );
                            }
                        });
                    }
                    Err(_) => {}
                }
            }
        }
    }
    let manager = PostgresConnectionManager::new(postgresstring.parse().unwrap(), NoTls);
    let pool = bb8::Pool::builder()
        .retry_connection(true)
        .connection_timeout(std::time::Duration::from_secs(3600))
        .idle_timeout(Some(std::time::Duration::from_secs(3600)))
        .build(manager)
        .await
        .unwrap();

    //let threadpool = ThreadPool::new(threadcount);
    let threaded_rt = runtime::Builder::new_multi_thread()
        .worker_threads(threadcount)
        .enable_all()
        .build()
        .unwrap();

    let mut handles = vec![];
    println!("run db upload now");
    println!("limittostaticfeed {:?}", &limittostaticfeed);
    let client = pool.get().await.unwrap();
    for (key, feed) in feedhashmap.to_owned().into_iter() {
        let pool = pool.to_owned();
        if feeds_to_discard.contains(&key.as_str()) {
            println!("Cancel SF bay override");
            continue;
        }
        if limittostaticfeed.is_some() || limittostaticfeed.as_ref().unwrap().as_str() != key.as_str() {
            continue;
            //println!("Cancelled because limit to static feed");
        }

        if soft_insert == Some(true) {
            let already_done = client.query(format!("SELECT onestop_feed_id, created_trips, updated_trips_time_ms FROM {schemaname}.feeds_updated WHERE onestop_feed_id = $1;").as_str(),
             &[&feed.id])
             .await.unwrap();
            if already_done.len() == 1 {
                continue;
            }
        }

        let bruhitfailed: Vec<OperatorPairInfo> = vec![];
        let listofoperatorpairs = feed_to_operator_pairs_hashmap
            .get(&feed.id)
            .unwrap_or_else(|| &bruhitfailed)
            .to_owned();
        let mut operator_pairs_hashmap: HashMap<String, Option<String>> = HashMap::new();
        for operator_pair in listofoperatorpairs {
            operator_pairs_hashmap.insert(operator_pair.operator_id, operator_pair.gtfs_agency_id);
        }
        //let items: Vec<String> = vec![];
        //let operator_id_list = feed_to_operator_hashmap.get(&key).unwrap_or_else(|| &items);
        handles.push(threaded_rt.spawn(async move 
            {
                //it timesout here a lot
                let client = pool.get().await.unwrap();
    
                //println!("Feed in future {}: {:#?}", key, feed);

                match feed.spec {
                    dmfr::FeedSpec::Gtfs => {
                        //println!("{:?}", feed.urls);
    
                        if feed.urls.static_current.is_some() {
                            //check if folder exists in the directory
                            //process and upload routes, stops, headways, and shapes etc into postgres
                            //calculate the bounds of the feed,
                            //upload the feed id metadata
    
                            let file_path = format!("gtfs_uncompressed/{}/", key);
    
                            if path_exists(&file_path) {
                                //feed exists
                                println!("Starting read for {}", &key);
    
                                let folder_size = get_size(&file_path).unwrap();
                                println!("size: {} kB", folder_size / 1000); 
    
                                let gtfs = gtfs_structures::GtfsReader::default().read_from_path(&file_path);
    
                                match gtfs {
                                Ok(gtfs) => {
    
                                    println!("read_duration: {:?}ms", gtfs.read_duration);
    
                                    println!(
                                        "there are {} stops in the gtfs",
                                        gtfs.stops.len()
                                    );
    
                                    println!(
                                        "there are {} routes in the gtfs",
                                        gtfs.routes.len()
                                    );
    
                                    let mut least_lat: Option<f64> = None;
                                    let mut least_lon: Option<f64> = None;
    
                                    let mut most_lat: Option<f64> = None;
                                    let mut most_lon: Option<f64> = None;

                                    let (stop_ids_to_route_types,stop_ids_to_route_ids) = make_hashmap_stops_to_route_types_and_ids(&gtfs);
    
                                    let (stop_id_to_children_ids, stop_ids_to_children_route_types) = make_hashmaps_of_children_stop_info(&gtfs,&stop_ids_to_route_types,&stop_ids_to_route_ids);

                                    //let timestarting = std::time::Instant::now();
    
                                    for (stop_id, stop) in &gtfs.stops {
                                        //check if least_lat has a value
    
                                        if (*stop).deref().longitude.is_some() {
                                            let stop_lon = (*stop).deref().longitude.unwrap();
                                            if stop_lon != 0.0 {
                                                if least_lon.is_some() {
                                                    if stop_lon < least_lon.unwrap() {
                                                        least_lon = Some(stop_lon);
                                                    }
                                                } else {
                                                    least_lon = Some(stop_lon);
                                                }
        
                                                if most_lon.is_some() {
                                                    if stop_lon > most_lon.unwrap() {
                                                        most_lon = Some(stop_lon);
                                                    }
                                                } else {
                                                    most_lon = Some(stop_lon);
                                                }
                                            }
                                        }
    
                                        if (*stop).deref().latitude.is_some() {
                                            let stop_lat = (*stop).deref().latitude.unwrap();
                                            if stop_lat != 0.0 {
                                                if least_lat.is_some() {
                                                    if stop_lat < least_lat.unwrap() {
                                                        least_lat = Some(stop_lat);
                                                    }
                                                } else {
                                                    least_lat = Some(stop_lat);
                                                }
        
                                                if most_lat.is_some() {
                                                    if stop_lat > most_lat.unwrap() {
                                                        most_lat = Some(stop_lat);
                                                    }
                                                } else {
                                                    most_lat = Some(stop_lat);
                                                }
                                            }
    
                                           
                                        }
                                    }
    
                                    let mut shape_to_color_lookup: BTreeMap<String, RGB<u8>> = BTreeMap::new();
                                    let mut shape_to_text_color_lookup: BTreeMap<String, RGB<u8>> = BTreeMap::new();
    
                                    for (trip_id, trip) in &gtfs.trips {
                                        if trip.shape_id.is_some() {
                                            if !shape_to_color_lookup.contains_key(&trip.shape_id.as_ref().unwrap().to_owned()) {
                                                if gtfs.routes.contains_key(&trip.route_id) {
                                                    let route = gtfs
                                                        .routes
                                                        .get(&trip.route_id)
                                                        .unwrap();
    
                                                    let color = colour_correction::fix_background_colour_rgb_feed_route(&feed.id,route.color,route);
    
                                                    shape_to_color_lookup.insert(
                                                    trip.shape_id.as_ref().unwrap().to_owned(),
                                                        color,
                                                    );
                                                    shape_to_text_color_lookup.insert(
                                                        trip.shape_id.as_ref().unwrap().to_owned(),
                                                        route.text_color,
                                                    );
                                                }
                                            }
                                        }
                                    }
    
                                   let prepared_shapes = client.prepare(format!("INSERT INTO {schemaname}.shapes
                                    (onestop_feed_id, shape_id, linestring, color, text_color, routes, route_type,route_label) 
                                    VALUES ($1, $2, $3, $4, $5, $6,$7,$8) ON CONFLICT (onestop_feed_id, shape_id) DO UPDATE set
                                    linestring = $3,
                                    color = $4,
                                    text_color = $5,
                                    routes = $6,
                                    route_type = $7,
                                    route_label = $8
                                    ;").as_str()).await.unwrap();

                                    for (shape_id, shape) in &gtfs.shapes {
                                        let mut route_ids: Vec<String> = match gtfs
                                        .trips
                                        .iter()
                                        .filter(|(trip_id, trip)| {
                                            trip.shape_id.is_some()
                                                && trip.shape_id.as_ref().unwrap()
                                                    == shape_id
                                        })
                                        .map(|(trip_id, trip)| trip.route_id.to_owned())
                                        .collect::<Vec<String>>()
                                        .as_slice()
                                    {
                                        [] => vec![],
                                        route_ids => route_ids.to_vec(),
                                    };
                                     if feed.id == "f-9qh-metrolinktrains" {
                                        let cleanedline = shape_id.to_owned().replace("in","").replace("out","");
                                       
                                        println!("cleanedline: {}", &cleanedline);
                                            let value = match cleanedline.as_str() {
                                                "91" => "91 Line",
                                                "IEOC" => "Inland Emp.-Orange Co. Line",
                                                "AV" => "Antelope Valley Line",
                                                "OC" => "Orange County Line",
                                                "RIVER" => "Riverside Line",
                                                "SB" => "San Bernardino Line",
                                                "VT" => "Ventura County Line",
                                                _ => "",
                                            };
                                            println!("real metrolink line {}", &value);
                                            if value != "" {
                                                route_ids.push(value.to_string());
                                            }
                                     }
                                     
                                     let route_ids:Vec<String> = route_ids.into_iter().unique().collect();
                                     let mut route_type_number = 3;
                                        if route_ids.len() > 0 {
                                            let route = gtfs.routes.get(&route_ids[0]);
                                            
                                            if route.is_some() {
                                                route_type_number = route_type_to_int(
                                                    &route.unwrap().route_type
                                                );
                                            }
                                        }
                                        let color_to_upload =
                                        match feed.id.as_str() {
                                            "f-9qh-metrolinktrains" => {
                                                if route_ids.len() > 0 {
                                                    let route = gtfs.routes.get(&route_ids[0]);
                                                    let color = route.unwrap().color;
                                                    format!(
                                                        "{:02x}{:02x}{:02x}",
                                                        color.r, color.g, color.b
                                                    )
                                                } else {
                                                    String::from("3a3a3a")
                                                }
                                            },
                                            _ => {
                                                match shape_to_color_lookup.get(shape_id) {
                                                    Some(color) => format!(
                                                        "{:02x}{:02x}{:02x}",
                                                        color.r, color.g, color.b
                                                    ),
                                                    None => String::from("3a3a3a"),
                                                }
                                            }
                                        };
                                        //bug "Line String must at least have 2 points"
                                        let preshape = shape
                                        .iter()
                                        .filter(|point| {
                                            match feed.id.as_str() {
                                                "f-9q5-metro~losangeles~rail" => {
                                                    //remove B/D railyard
                                                    match color_to_upload.as_str() {
                                                        "eb131b" => {
                                                            point.longitude < -118.2335698
                                                        }
                                                        "a05da5" => {
                                                            point.longitude < -118.2335698
                                                        },
                                                        "e470ab" => {
                                                            point.latitude > 33.961543
                                                        }
                                                        _ => true,
                                                    }
                                                }
                                                _ => true,
                                            }
                                        })
                                        .filter(|point| {
                                            match route_ids.len() {
                                                1 => {
                                                    //remove B/D railyard
                                                    match route_ids[0].as_str() {
                                                        "807" => {
                                                            point.latitude > 33.961543
                                                        }
                                                        _ => true,
                                                    }
                                                }
                                                _ => true,
                                            }
                                        })
                                    ;
                                        if preshape.to_owned().count() < 2 {
                                            println!("Shape {} has less than 2 points", shape_id);
                                            continue;
                                        }
                                        let linestring = ewkb::LineStringT {
                                            srid: Some(4326),
                                            points: 
                                                preshape.map(|point| ewkb::Point {
                                                    x: point.longitude,
                                                    y: point.latitude,
                                                    srid: Some(4326),
                                                })
                                                .collect(),
                                        };
    
                                        let text_color = match feed.id.as_str() {
                                            "f-9qh-metrolinktrains" => {
                                                if route_ids.len() > 0 {
                                                    let route = gtfs.routes.get(&route_ids[0]);
                                                    let text_color = route.unwrap().text_color;
                                                    format!(
                                                        "{:02x}{:02x}{:02x}",
                                                        text_color.r, text_color.g, text_color.b
                                                    )
                                                } else {
                                                    String::from("ffffff")
                                                }
                                            },
                                            "f-9-amtrak~amtrakcalifornia~amtrakcharteredvehicle" => {
                                                String::from("ffffff")
                                            },
                                            _ => {
                                                match shape_to_text_color_lookup.get(shape_id) {
                                                    Some(color) => format!(
                                                        "{:02x}{:02x}{:02x}",
                                                        color.r, color.g, color.b
                                                    ),
                                                    None => String::from("000000"),
                                                }
                                            }
                                        };
    
                                           // println!("uploading shape {:?} {:?}", &feed.id, &shape_id);
                                           let route_label:String = route_ids.iter().map(|route_id| {
                                            let route = gtfs.routes.get(route_id);
                                            if route.is_some() {
                                                if route.unwrap().short_name.as_str() == "" {
                                                  if route.unwrap().long_name.as_str() == "" {
                                                    return route_id.to_string();
                                                  } else {
                                                    return route.unwrap().long_name.to_owned()
                                                    .replace("-16168","")
                                                    .replace("Counterclockwise", "ACW").replace("counterclockwise", "ACW").replace("clockwise", "CW").replace("Clockwise", "CW");
                                                  }
                                                } else {
                                                    return route.unwrap().short_name.to_owned()
                                                    .replace("-16168","")
                                                    .replace("Counterclockwise", "ACW").replace("counterclockwise", "ACW").replace("clockwise", "CW").replace("Clockwise", "CW");
                                                }
                                                
                                            } else {
                                                return route_id.to_string();
                                            }
                                           }).collect::<Vec<String>>().join(",").as_str().replace("Orange County","OC").replace("Inland Empire", "IE").to_string();
    
                                        client.query(&prepared_shapes,
                                     &[
                                        &feed.id,
                                        &shape_id, 
                                     &linestring,
                                     &colour_correction::fix_background_colour(color_to_upload.as_str()),
                                     &colour_correction::fix_foreground_colour(color_to_upload.as_str(),text_color.as_str()),
                                     &route_ids,
                                     //add route type here
                                    &route_type_number,
                                    &route_label
                                     ]).await.unwrap();
                                    }

                                    let routes: HashMap<(String, String), (&Route, &PooledConnection<PostgresConnectionManager<NoTls>>)> = gtfs.routes.iter()
                                        .map(|(key, route)| ((key.to_owned(), feed.id.to_owned()), (route, &client))).collect();
                                    let routes_clone = routes.to_owned();
                                    let route_workers = routes_clone.into_iter().map( |((route_id, feed_id), (route, client))| async move {
                                        let route_type_number = route_type_to_int(&route.route_type);
                                        let shapes_per_route: HashMap<String, Vec<String>> = HashMap::new();
                                        let shape_id_array: Vec<String> =
                                            match shapes_per_route.get(&route_id) {
                                                Some(shape_list) => shape_list.to_owned(),
                                                None => vec![],
                                            };
                                        let shape_id_array:Vec<String> = shape_id_array.into_iter().unique().collect();
                                        //println!("uploading route {:?} {}", &feed.id , &route_id);
                                        let route_prepared = client.prepare(format!("INSERT INTO {schemaname}.routes
                                        (
                                            route_id,
                                            onestop_feed_id,
                                            short_name,
                                            long_name,
                                            gtfs_desc,
                                            route_type,
                                            url,
                                            agency_id,
                                            gtfs_order,
                                            color,
                                            text_color,
                                            continuous_pickup,
                                            continuous_drop_off,
                                            shapes_list
                                        )
                                        VALUES (
                                            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
                                        ) ON CONFLICT (onestop_feed_id, route_id) do update set 
                                        color = $10,
                                        text_color = $11;
                                        ").as_str()).await.unwrap();
                                        let long_name = titlecase_process_new_nooption(&route.long_name);
                                        client.query(
                                            &route_prepared,
                                            &[
                                                &route_id,
                                                &feed_id,
                                                &route.short_name,
                                                &long_name,
                                                &route.desc,
                                                &route_type_number,
                                                &route.url,
                                                &route.agency_id,
                                                &i32::try_from(route.order.unwrap_or_else(|| 0)).ok(),
                                                &(colour_correction::fix_background_colour_rgb_feed_route(&feed_id,route.color, &route).to_string()),
                                                &(colour_correction::fix_foreground_colour_rgb_feed(&feed_id, route.color, route.text_color).to_string()),
                                                &(match route.continuous_pickup {
                                                    ContinuousPickupDropOff::Continuous => 0,
                                                    ContinuousPickupDropOff::NotAvailable => 1,
                                                    ContinuousPickupDropOff::ArrangeByPhone => 2,
                                                    ContinuousPickupDropOff::CoordinateWithDriver => 3,
                                                    ContinuousPickupDropOff::Unknown(i) => i,
                                                }),
                                                &(match route.continuous_drop_off {
                                                    ContinuousPickupDropOff::Continuous => 0,
                                                    ContinuousPickupDropOff::NotAvailable => 1,
                                                    ContinuousPickupDropOff::ArrangeByPhone => 2,
                                                    ContinuousPickupDropOff::CoordinateWithDriver => 3,
                                                    ContinuousPickupDropOff::Unknown(i) => i,
                                                }),
                                                &shape_id_array,
                                            ],
                                        ).await.unwrap();
                                    });
                                    for worker in route_workers {
                                        let _ = tokio::join!(worker);
                                    }
                                    println!("Uploading {} trips", gtfs.trips.len());
                                     
                                    let time = std::time::Instant::now();
                                    if skiptrips == false {
                                        let trips: HashMap<(String, String), (&Trip, &PooledConnection<PostgresConnectionManager<NoTls>>)> = gtfs.trips.iter()
                                        .map(|(key, trip)| ((key.to_owned(), feed.id.to_owned()), (trip, &client))).collect();
                                        let trips_clone = trips.to_owned();
                                        let trips_workers = trips_clone.into_iter().map( |((trip_id, feed_id), (trip, client))| async move {
                                            let statement = client.prepare(format!("INSERT INTO {schemaname}.trips 
                                            (onestop_feed_id, trip_id, service_id, route_id, trip_headsign, trip_short_name, shape_id, has_stop_headsign, stop_headsigns)
                                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (onestop_feed_id, trip_id) do update set
                                             service_id = $3,
                                             route_id = $4,
                                                trip_headsign = $5,
                                                trip_short_name = $6,
                                                shape_id = $7,
                                                has_stop_headsign = $8,
                                                stop_headsigns = $9
                                             ;").as_str()).await.unwrap();
                                            let stoptimestatement = client.prepare(
                                                format!("INSERT INTO {schemaname}.stoptimes 
                                                (onestop_feed_id, trip_id, stop_id, stop_sequence, 
                                                    arrival_time, departure_time, stop_headsign, point) 
                                                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING;").as_str()).await.unwrap();
                                            
                                            let trip_headsign = titlecase_process_new(trip.trip_headsign.as_ref());
                                            //calculate if any stop time has a stop headsign
                                            let has_stop_headsign = trip.stop_times.iter().any(|stoptime| {
                                                stoptime.stop_headsign.is_some()
                                            });
                                            let mut stop_headsigns_for_trip = trip.stop_times.iter().map(|stoptime| {
                                                stoptime.stop_headsign.as_ref()
                                            }).collect::<Vec<Option<&String>>>().into_iter().unique().collect::<Vec<Option<&String>>>();
                                            //dedup
                                            stop_headsigns_for_trip.dedup();
                                            let stop_headsigns_for_trip = stop_headsigns_for_trip;

                                            client.query(
                                                &statement,
                                        &[
                                                    &feed_id,
                                                    &trip.id,
                                                    &trip.service_id,
                                                    &trip.route_id,
                                                    &trip_headsign,
                                                    &trip.trip_short_name,
                                                    &trip.shape_id,
                                                    &has_stop_headsign,
                                                    &stop_headsigns_for_trip
                                                ],
                                            ).await.unwrap();

                                            for stoptime in &trip.stop_times {

                                                if stoptime.stop.latitude.is_some() && stoptime.stop.longitude.is_some() {
                                                    let point = ewkb::Point {
                                                        x: stoptime.stop.longitude.unwrap(),
                                                        y: stoptime.stop.latitude.unwrap(),
                                                        srid: Some(4326),
                                                    };
                                            

                                                    let stop_headsign:Option<String> = titlecase_process_new(stoptime.stop_headsign.as_ref());
                                                
                                                    if stoptime.arrival_time.is_some() && stoptime.departure_time.is_some() {
                                                        client
                                                    .query(
                                                        &stoptimestatement,
                                                        &[
                                                            &feed_id,
                                                            &trip.id,
                                                            &stoptime.stop.id,
                                                            &(stoptime.stop_sequence as i32),
                                                            &toi64(&stoptime.arrival_time),
                                                            &toi64(&stoptime.departure_time),
                                                            &stop_headsign,
                                                            &point
                                                        ],
                                                    ).await.unwrap();
                                                    }    
                                                }
                                            }
                                        });
                                        for worker in trips_workers {
                                            let _ = tokio::join!(worker);
                                        }
                                                      
                                    println!("{} with {} trips took {}ms", feed.id, gtfs.trips.len(), time.elapsed().as_millis());
                                    }

                                    //Pre-process stops, identify children stops with the same name

                                    //(lat,lon) -> stop_id
                                    let mut hashmap_of_coords_to_stops: HashMap<(OrderedFloat<f64>,OrderedFloat<f64>),Vec<String>> = HashMap::new();

                                    for (stop_id,stop) in &gtfs.stops {
                                        if stop.latitude.is_some() && stop.longitude.is_some() {
                                            hashmap_of_coords_to_stops.entry((
                                                OrderedFloat(stop.latitude.unwrap()), OrderedFloat(stop.longitude.unwrap())
                                            ))
                                            .and_modify(|array| array.push(stop_id.to_owned()))
                                            .or_insert(vec![stop_id.to_owned()]);
                                        }
                                    }

                                    let hashmap_of_coords_to_stops: HashMap<(OrderedFloat<f64>, OrderedFloat<f64>), Vec<String>> = hashmap_of_coords_to_stops
                                    .into_iter()
                                    .filter(|(k,v)| v.len() >= 2)
                                    .collect::<HashMap<(OrderedFloat<f64>,OrderedFloat<f64>),Vec<String>>>();

                                    println!("{} Stops that are duplicate points", hashmap_of_coords_to_stops.len());

                                    //criteria for hiding
                                    //if the 2 stops share the same name, mark them aliases. IF there is a child and parent, mark the children as hidden, pointing torwards the parent station.
                                    //This resolves a bug (poorly designed GTFS Schedule files) where Los Angeles Metro places 2 rail station stops at the same coordinates.
                                  
                                    let mut hashmap_stops_dedup_meta: HashMap<String, (bool, Arc<Vec<String>>)> = HashMap::new();

                                    for (_,vec_of_stop_ids) in hashmap_of_coords_to_stops {
                                        let arc_of_stop_ids = Arc::new(vec_of_stop_ids);

                                        //lookup of each group and categorise the stops
                                        let list_of_stops = arc_of_stop_ids.iter().map(|stop_id| gtfs.stops.get(stop_id).unwrap().to_owned()).collect::<Vec<Arc<gtfs_structures::Stop>>>();
                                        let dont_hide_this_stop_candidates = list_of_stops.iter().filter(|stop| stop.parent_station.is_none()).map(|stop| stop.to_owned()).collect::<Vec<Arc<gtfs_structures::Stop>>>();
                                       // let dont_hide_this_stop_candidates_stop_ids = dont_hide_this_stop_candidates.iter().map(|stop| stop.id.to_owned()).collect::<HashSet<String>>();
                                       let dont_hide_this_stop_candidates_names = dont_hide_this_stop_candidates.iter().map(|stop| stop.name.to_owned()).collect::<HashSet<String>>();


                                        for stop in list_of_stops {
                                            
                                        let mut hidden_stops = false;

                                        if stop.parent_station.is_none() {  
                                            hidden_stops = false;
                                        } else {
                                            //todo! implement search for nearby stops with the same name, probably using hexagonal hashing structure search
                                            //prob not a good idea, since NYC has a good reason
                                            // Thank you professor Michael Goodrich, I am forever blessed with complexity analysis of algorithms
                                            if let gtfs_structures::LocationType::StationEntrance = stop.location_type {} else {
                                                //prevents "7th St/Metro Center" from shadowing "7th St/Metro Center - Metro A & E Lines"
                                                if dont_hide_this_stop_candidates_names.contains(&stop.name) {
                                                    hidden_stops = true
                                                }
                                            }
                                        }

                                        hashmap_stops_dedup_meta.insert(stop.id.to_owned(), (hidden_stops,arc_of_stop_ids.to_owned()));
                                        }
                                    }

                                    let stopstatement = client.prepare(format!(
                                        "INSERT INTO {schemaname}.stops
                                     (onestop_feed_id, gtfs_id, name, displayname, code, gtfs_desc, point, route_types, routes, location_type, parent_station, children_ids, children_route_types, hidden, location_alias)
                                           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) ON CONFLICT DO NOTHING;"
                                    ).as_str()).await.unwrap();
                                    for (stop_id, stop) in &gtfs.stops {
                                       if stop.latitude.is_some() && stop.longitude.is_some() {
                                        let point = ewkb::Point {
                                            x: stop.longitude.unwrap(),
                                            y: stop.latitude.unwrap(),
                                            srid: Some(4326),
                                        };
                                        let name = titlecase_process_new_nooption(&stop.name);
                                        let displayname = name.to_owned().to_string().replace(" Station","").replace("Northbound","N.B.").replace("Eastbound","E.B.").replace("Southbound","S.B.").replace("Westbound","W.B.");

                                        let fetch_of_dedup = hashmap_stops_dedup_meta.get(&stop.id);

                                        let hidden_stop = match fetch_of_dedup {
                                            Some(fetch_of_dedup) => fetch_of_dedup.0,
                                            _ => false
                                        };

                                        let alias_names: Option<&Vec<String>> = match fetch_of_dedup {
                                            Some(fetch_of_dedup) => Some(fetch_of_dedup.1.deref()),
                                            _ => None
                                        };

                                        client.query(&stopstatement, &[
                                            &feed.id,
                                            &stop.id,
                                            &name,
                                            &displayname,
                                            &stop.code,
                                            &stop.description,
                                            &point,
                                            &stop_ids_to_route_types.get(&stop.id),
                                            &stop_ids_to_route_ids.get(&stop.id),
                                            &location_type_conversion(&stop.location_type),
                                            &stop.parent_station,
                                            &stop_id_to_children_ids.get(&stop.id),
                                            &stop_ids_to_children_route_types.get(&stop.id),
                                            &hidden_stop,
                                            &alias_names
                                        ]).await.unwrap();
                                       }
                                    }

                                    let start_hull_time = chrono::prelude::Utc::now().timestamp_nanos_opt().unwrap();
                                    //convex hull calcs
                                    let mut shape_points = gtfs.shapes.iter().map(|(a,b)| b)
                                    .flat_map(|s| s.iter())
                                    .map(|s| (s.longitude, s.latitude))
                                    .collect::<Vec<(f64, f64)>>();
                                    shape_points.par_sort_unstable_by(|a, b| match a.0.partial_cmp(&b.0) {
                                        Some(ord) => ord,
                                        None => a.1.partial_cmp(&b.1).unwrap(),
                                    });
                                    shape_points.dedup();
                                    let shape_points = shape_points;
                                    let hull = convex_hull::convex_hull(&shape_points);
                                    let stop_hull_time = chrono::prelude::Utc::now().timestamp_nanos_opt().unwrap();
                                    
                                    let num_of_points_polygon = hull.exterior().coords_count();

                                    println!("Convex Hull Algo for {} took {}μs", feed.id, (stop_hull_time - start_hull_time) / 1000);
                                    println!("{} points", shape_points.len());
                                    
                                    let hull_postgres = match num_of_points_polygon {
                                        0 => geo::Polygon::new(
                                            geo::LineString::from(vec![
                                                (most_lon.unwrap(), most_lat.unwrap()),
                                             (most_lon.unwrap(), least_lat.unwrap()),
                                             (least_lon.unwrap(), least_lat.unwrap()), (least_lon.unwrap(), most_lat.unwrap()), (most_lon.unwrap(), most_lat.unwrap())]),vec![]).to_postgis_wgs84(),
                                        _ => hull.to_postgis_wgs84()
                                    };

                                    if gtfs.routes.len() > 0 as usize {
                                        let _ = client.query(
                                            format!("INSERT INTO {schemaname}.static_feeds (onestop_feed_id, max_lat, max_lon, min_lat, min_lon, operators, operators_to_gtfs_ids, hull)
                                        
                                            VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (onestop_feed_id) do update set operators = $6, operators_to_gtfs_ids = $7, hull = $8;").as_str(), &[
                                        &feed.id,
                                        &most_lat,
                                        &most_lon,
                                        &least_lat,
                                        &least_lon,
                                        &operator_pairs_hashmap.par_iter().map(|(a,b)| a).collect::<Vec<&String>>(),
                                        &operator_pairs_hashmap,
                                        &hull_postgres
                                    ]).await.unwrap();
                                    if skiptrips == false {
                                        
                                        //get current unix timestamp
                                        let since_the_epoch = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .expect("Time went backwards");
                                        let in_ms = since_the_epoch.as_millis();
                                        client.execute(
                                            format!(
                                                "INSERT INTO {schemaname}.feeds_updated (onestop_feed_id, created_trips, updated_trips_time_ms) VALUES ($1, $2, $3) ON CONFLICT (onestop_feed_id) DO UPDATE SET created_trips = $2, updated_trips_time_ms = $3;"
                                            ).as_str()
                                            , &[
                                            &feed.id,
                                            &true,
                                            &(in_ms as i64)
                                        ]).await.unwrap();

                                        client.execute(
                                            format!(
                                                "DELETE FROM {schemaname}.gtfs_errors WHERE onestop_feed_id = $1;"
                                            ).as_str()
                                            , &[
                                            &feed.id,
                                        ]).await.unwrap();
                                    }
                                    }
                                },
                                Err(gtfs_err) => {
                                    println!("{} is not a valid gtfs feed", &key);
                                    println!("{:?}", gtfs_err);
                                    //we should save this in some database
                                    let errormsg = format!("{:#?}", gtfs_err);
                                    client.query(format!("INSERT INTO {schemaname}.gtfs_errors (onestop_feed_id, error) VALUES ($1, $2) ON CONFLICT (onestop_feed_id) DO UPDATE SET error = $2;").as_str(), &[
                                        &feed.id,
                                        &errormsg
                                    ]).await.unwrap();
                                }
                            }
                            }
                        }
                    },
                    dmfr::FeedSpec::GtfsRt => {
                            client.query(format!("INSERT INTO {schemaname}.realtime_feeds (onestop_feed_id, name, operators, operators_to_gtfs_ids)
                            VALUES ($1, $2, $3, $4) ON CONFLICT do nothing;").as_str(), &[
                            &feed.id,
                            &feed.name,
                            &operator_pairs_hashmap.iter().map(|(a,b)| a).collect::<Vec<&String>>(),
                            &operator_pairs_hashmap
                        ]).await.unwrap();
                    },
                    _ => {
                        //do nothing
                        println!("skipping {}, does not match dmfr feed spec", &key);
                    }
                }
        }));
    }
    futures::future::join_all(handles).await;
    println!("Done ingesting all gtfs statics");
    println!("number of operators: {}", operatorhashmap.len());
    for (operator_id, operator) in operatorhashmap {
        //println!("{:?}", operator);
        /*
            onestop_operator_id text PRIMARY KEY,
        name text,
        gtfs_static_feeds text[],
             */
        let empty_vec: Vec<dmfr::OperatorAssociatedFeedsItem> = vec![];
        let listoffeeds = operator_to_feed_hashmap
            .get(&operator_id)
            .unwrap_or_else(|| &empty_vec)
            .to_owned();
        let mut gtfs_static_feeds: HashMap<String, Option<String>> = HashMap::new();
        let mut gtfs_realtime_feeds: HashMap<String, Option<String>> = HashMap::new();
        let mut simplified_array_static: Vec<String> = vec![];
        let mut simplified_array_realtime: Vec<String> = vec![];
        for x in listoffeeds {
            //get type
            if x.feed_onestop_id.is_some() {
                if feedhashmap.contains_key((&x.feed_onestop_id).as_ref().unwrap()) {
                    let feed = feedhashmap
                        .get((&x.feed_onestop_id).as_ref().unwrap())
                        .unwrap();
                    match feed.spec {
                        dmfr::FeedSpec::Gtfs => {
                            if !feeds_to_discard
                                .contains(&(&x.feed_onestop_id).as_ref().unwrap().as_str())
                            {
                                gtfs_static_feeds.insert(
                                    x.feed_onestop_id.to_owned().unwrap(),
                                    x.gtfs_agency_id,
                                );
                                simplified_array_static.push(x.feed_onestop_id.to_owned().unwrap());
                            }
                        }
                        dmfr::FeedSpec::GtfsRt => {
                            gtfs_realtime_feeds
                                .insert(x.feed_onestop_id.to_owned().unwrap(), x.gtfs_agency_id);
                            simplified_array_realtime.push(x.feed_onestop_id.to_owned().unwrap());
                        }
                        _ => {}
                    }
                }
            }
        }
        client
            .query(
                format!(
                    "INSERT INTO {schemaname}.operators 
                (onestop_operator_id, 
                    name, 
                    gtfs_static_feeds, 
                    gtfs_realtime_feeds, 
                    static_onestop_feeds_to_gtfs_ids, 
                    realtime_onestop_feeds_to_gtfs_ids)
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING;"
                )
                .as_str(),
                &[
                    &operator.onestop_id,
                    &operator.name,
                    &simplified_array_static,
                    &simplified_array_realtime,
                    &gtfs_static_feeds,
                    &gtfs_realtime_feeds,
                ],
            )
            .await
            .unwrap();
    }
    println!("Done ingesting all operators");
    println!("adding extra lines");
    let realtime_override_file = std::fs::File::open("add-realtime-feeds.csv").unwrap();
    let mut realtime_override_reader =
        csv::Reader::from_reader(std::io::BufReader::new(realtime_override_file));
    let realtime_overrides = realtime_override_reader
        .records()
        .filter(|x| x.is_ok())
        .map(|x| RealtimeOverride {
            realtimeid: x.as_ref().unwrap().to_owned()[0].to_string(),
            operatorid: x.as_ref().unwrap().to_owned()[1].to_string(),
        })
        .collect::<Vec<RealtimeOverride>>();
    for realtime_override in realtime_overrides {
        client.query(format!("UPDATE {schemaname}.operators SET gtfs_realtime_feeds = 
        (select array_agg(distinct e) from unnest(gtfs_realtime_feeds || '{{{0}}}') e),
        realtime_onestop_feeds_to_gtfs_ids = realtime_onestop_feeds_to_gtfs_ids || '{0}=>null' :: hstore WHERE onestop_operator_id = $1", &realtime_override.realtimeid).as_str(), &[
        &realtime_override.operatorid
    ]).await.unwrap();

        client.query(
        format!("UPDATE {schemaname}.realtime_feeds SET operators = (select array_agg(distinct e) from unnest(operators || '{{{1}}}') e), operators_to_gtfs_ids = operators_to_gtfs_ids || '{1}=>null' :: hstore WHERE onestop_feed_id = '{0}'", 
            &realtime_override.realtimeid,
            &realtime_override.operatorid,
        )
        .as_str(),&[
        ]).await.unwrap();
    }

    println!("Waiting for {} seconds", 1);
    std::thread::sleep(std::time::Duration::from_secs(1));

    Ok(())
}
