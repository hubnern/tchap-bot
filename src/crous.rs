use std::{fmt::Display, sync::LazyLock};

use chrono::Local;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Clone)]
pub struct Dish {
    pub style: String,
    pub food: String,
}

const HAUT_CARRE: u32 = 411;

static CACHE: LazyLock<Mutex<(String, Vec<Dish>)>> = LazyLock::new(|| Mutex::new((String::new(), vec![])));

pub async fn fetch_restaurant_menus() -> reqwest::Result<Vec<Dish>> {
    let now = Local::now().format("%Y-%m-%d").to_string();
    let mut cached = CACHE.lock().await;
    info!("date now is `{}` and menu date is `{}`", now, cached.0);
    if now == cached.0 {
        Ok(cached.1.clone())
    } else {
        match inner_fetch_restaurant_menus().await {
            Ok((date, dishes)) => {
                *cached = (date, dishes.clone());
                Ok(dishes)
            }
            Err(e) => Err(e),
        }
    }
}
async fn inner_fetch_restaurant_menus() -> reqwest::Result<(String, Vec<Dish>)> {
    info!("Fetching menu from the crous api");
    let url = "http://webservices-v2.crous-mobile.fr/feed/bordeaux/externe/crous-bordeaux.min.json";
    let cleaned_json: String = reqwest::get(url)
        .await?
        .text()
        .await?
        .chars()
        // The crous json contains control chars (like tabs) but is an invalid json :(
        .filter(|c| !c.is_control())
        .collect();

    if let Ok(feed) = serde_json::from_str::<CrousFeed>(&cleaned_json) {
        let first_menu = feed
            .restaurants
            .iter()
            .find(|r| r.id == HAUT_CARRE)
            .expect("the haut carre restaurant should be here")
            .menus
            .first()
            .expect("there should be at least one menu");
        let date = first_menu.date.clone();
        let meals = first_menu
            .meal
            .first()
            .expect("there should be at least one meal")
            .foodcategory
            .iter()
            .map(|d| Dish {
                style: d.name.clone(),
                food: d
                    .dishes
                    .iter()
                    .map(|e| {
                        e.name
                            .trim()
                            .trim_end_matches('.')
                            .replace(" ,", ", ")
                            .replace("Ou", "")
                            .replace("Où", "")
                            .trim()
                            .to_string()
                    })
                    .filter(|e| !e.is_empty())
                    .collect::<Vec<String>>()
                    .join(", "),
            })
            .collect();
        Ok((date, meals))
    } else {
        eprintln!("Erreur de lecture de la liste des restaurants");
        Ok((String::new(), vec![]))
    }
}

#[derive(Serialize, Deserialize)]
struct CrousFeed {
    restaurants: Vec<CrousFeedRestaurant>,
}

#[derive(Serialize, Deserialize)]
struct CrousFeedRestaurant {
    id: u32,
    title: String,
    area: String,
    #[serde(rename = "type")]
    r#type: String,
    menus: Vec<CrousFeedMenu>,
}

impl Display for CrousFeedRestaurant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title)
    }
}

#[derive(Serialize, Deserialize)]
struct CrousFeedMenu {
    date: String,
    meal: Vec<CrousFeedMeal>,
}

#[derive(Serialize, Deserialize)]
struct CrousFeedMeal {
    name: String,
    foodcategory: Vec<CrousFeedCategory>,
}

#[derive(Serialize, Deserialize)]
struct CrousFeedCategory {
    name: String,
    dishes: Vec<CrousFeedDish>,
}

#[derive(Serialize, Deserialize)]
struct CrousFeedDish {
    name: String,
}
