#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use serde_json::{Value, json};

pub(crate) const SIXTY_MINUTE_DELIVERY: &str = "sixty-min-delivery";
pub(crate) const ONE_DAY_DELIVERY: &str = "one-day-delivery";

pub(crate) fn verify_cell_body(mobile_number: &str) -> Value {
    json!({ "mobileNumber": mobile_number })
}

pub(crate) fn request_mobile_otp_body(mobile_number: &str) -> Value {
    json!({ "mobileNumber": mobile_number })
}

pub(crate) fn verify_otp_body(mobile_number: &str, otp: &str, otp_reference: &str) -> Value {
    json!({
        "OTP": otp,
        "OTPReference": otp_reference,
        "mobileNumber": mobile_number,
        "isEmail": false,
    })
}

pub(crate) fn empty_body() -> Value {
    json!({})
}

pub(crate) fn decrypt_scheme_body(scheme: &Value) -> serde_json::Result<Value> {
    Ok(json!({ "hash": serde_json::to_string(scheme)? }))
}

pub(crate) fn sixty60_terms_consent_body(uuid: &str) -> Value {
    json!({
        "data": {
            "uuid": uuid,
            "consentPayload": [{
                "consentTemplateId": "sixty60-za-termsandconditions",
                "consentTemplateVersion": "1",
                "granted": true,
            }]
        }
    })
}

pub(crate) fn xtra_savings_rewards_consent_body(uuid: &str) -> Value {
    json!({
        "data": {
            "uuid": uuid,
            "consentPayload": [{
                "consentTemplateId": "checkers-za-rewards-consent",
                "consentTemplateVersion": "1",
                "granted": true,
            }]
        }
    })
}

pub(crate) fn verify_date_of_birth_body(date_of_birth: &str, scheme: &Value) -> Value {
    json!({ "dateOfBirth": date_of_birth, "scheme": scheme })
}

pub(crate) fn validate_id_or_passport_body(id_or_passport: &str, uid: &str) -> Value {
    json!({ "idNumberOrPassportNumber": id_or_passport, "uid": uid })
}

pub(crate) fn update_customer_id_or_passport_body(payload: &Value) -> Value {
    json!({ "payloadObject": payload })
}

pub(crate) fn get_user_profile_body() -> Value {
    json!({ "bypassHash": false, "isLogin": true })
}

pub(crate) fn get_on_demand_profile_body(uid: &str) -> Value {
    json!({ "uid": uid })
}

pub(crate) fn get_user_addresses_body(address_id: &str) -> Value {
    json!({ "addressId": address_id })
}

pub(crate) fn fetch_store_contexts_body(
    delivery_address: &Value,
    sixty60_profile: &Value,
) -> Value {
    json!({
        "address": delivery_address,
        "email": str_at(sixty60_profile, "/email"),
        "mobileNumber": str_at(sixty60_profile, "/mobile"),
        "userId": str_at(sixty60_profile, "/id"),
        "update": true,
        "acceptedLimitedExperience": Value::Null,
    })
}

pub(crate) fn catalogue_search_body(store_contexts: &Value, query: &str) -> Value {
    json!({
        "storeContexts": store_contexts,
        "filterData": {
            "filter": {
                "showAllDisplayVariants": false,
                "showNotRangedProducts": false,
                "productListSource": { "search": query },
                "paginationOptions": { "page": 0, "pageSize": 16 },
                "filterOptions": {
                    "filterIds": [],
                    "dealsOnly": false,
                    "brandOptions": [],
                    "departmentOptions": [],
                    "serviceOptions": [],
                    "facetOptions": []
                },
                "sortOptions": Value::Null
            },
            "displayOptions": { "includeDisplayCategoryTree": false }
        }
    })
}

pub(crate) fn catalogue_product_ids_body(store_contexts: &Value, product_ids: &[&str]) -> Value {
    json!({
        "storeContexts": store_contexts,
        "filterData": {
            "filter": {
                "showAllDisplayVariants": true,
                "showNotRangedProducts": true,
                "productListSource": { "productIds": product_ids },
                "paginationOptions": { "page": 0, "pageSize": product_ids.len().max(1) },
                "filterOptions": {
                    "filterIds": [],
                    "dealsOnly": false,
                    "brandOptions": [],
                    "departmentOptions": [],
                    "serviceOptions": [],
                    "facetOptions": []
                },
                "sortOptions": Value::Null
            },
            "displayOptions": { "includeDisplayCategoryTree": false }
        }
    })
}

pub(crate) fn fetch_cart_body(store_contexts: &Value) -> Value {
    json!({
        "params": {
            "storeContexts": store_contexts,
            "serviceOptionIds": [SIXTY_MINUTE_DELIVERY, ONE_DAY_DELIVERY]
        }
    })
}

pub(crate) fn update_cart_payload_body(payload: &Value, is_naive_update: bool) -> Value {
    json!({
        "payload": payload,
        "isNaiveUpdate": is_naive_update,
    })
}

pub(crate) fn cart_items_from_fetch_response(response: &Value) -> Vec<Value> {
    response
        .get("carts")
        .and_then(Value::as_array)
        .map(|carts| {
            carts
                .iter()
                .map(|cart| cart.get("item").cloned().unwrap_or_else(|| cart.clone()))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn cart_response_contains_product(response: &Value, product_id: &str) -> bool {
    cart_items_from_fetch_response(response).iter().any(|cart| {
        cart.get("lineItems")
            .and_then(Value::as_array)
            .is_some_and(|lines| {
                lines.iter().any(|line| {
                    str_at(line, "/productId").as_deref() == Some(product_id)
                        || str_at(line, "/product/id").as_deref() == Some(product_id)
                })
            })
    })
}

pub(crate) fn build_cart_update_payload(
    carts: &[Value],
    product: &Value,
    service_option_id: &str,
    quantity: u32,
    delivery_address_id: &str,
    store_contexts: &Value,
    line_item_id: &str,
    selected_weight_option_index: Option<usize>,
) -> Result<Value, String> {
    let mut carts = carts.to_vec();
    upsert_product_line_item(
        &mut carts,
        product,
        service_option_id,
        quantity.max(1),
        line_item_id,
        selected_weight_option_index,
    )?;
    Ok(json!({
        "carts": carts.iter().map(cart_update_item).collect::<Vec<_>>(),
        "deliveryAddressId": delivery_address_id,
        "storeContexts": store_contexts,
    }))
}

fn upsert_product_line_item(
    carts: &mut Vec<Value>,
    product: &Value,
    service_option_id: &str,
    quantity: u32,
    line_item_id: &str,
    selected_weight_option_index: Option<usize>,
) -> Result<(), String> {
    if !carts.iter().any(|cart| {
        str_at(cart, "/serviceOptionId")
            .as_deref()
            .is_some_and(|id| id == service_option_id)
    }) {
        carts.push(empty_cart(service_option_id));
    }

    let product_id =
        str_at(product, "/id").ok_or_else(|| format!("product has no id: {product}"))?;
    let cart = carts
        .iter_mut()
        .find(|cart| str_at(cart, "/serviceOptionId").as_deref() == Some(service_option_id))
        .ok_or_else(|| "no cart for product service option".to_string())?;
    let line_items = ensure_line_items(cart)?;
    if let Some(existing) = line_items
        .iter_mut()
        .find(|line| str_at(line, "/productId").as_deref() == Some(product_id.as_str()))
    {
        let current = existing
            .get("quantity")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if let Some(map) = existing.as_object_mut() {
            map.insert("quantity".into(), json!(current + u64::from(quantity)));
            map.insert("serviceOptionId".into(), json!(service_option_id));
        }
        return Ok(());
    }

    line_items.push(line_item_from_product(
        product,
        &product_id,
        service_option_id,
        quantity,
        line_item_id,
        selected_weight_option_index,
    )?);
    Ok(())
}

fn ensure_line_items(cart: &mut Value) -> Result<&mut Vec<Value>, String> {
    let map = cart
        .as_object_mut()
        .ok_or_else(|| "cart is not an object".to_string())?;
    if !map.get("lineItems").is_some_and(Value::is_array) {
        map.insert("lineItems".to_string(), Value::Array(vec![]));
    }
    map.get_mut("lineItems")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "cart lineItems is not an array".to_string())
}

fn empty_cart(service_option_id: &str) -> Value {
    json!({
        "id": "",
        "serviceOptionId": service_option_id,
        "lineItems": []
    })
}

fn line_item_from_product(
    product: &Value,
    product_id: &str,
    service_option_id: &str,
    quantity: u32,
    line_item_id: &str,
    selected_weight_option_index: Option<usize>,
) -> Result<Value, String> {
    let price = product_price_cents(product)?;
    let selected_weight_range = product
        .get("variableWeightOptions")
        .and_then(Value::as_array)
        .and_then(|options| {
            let index = selected_weight_option_index.unwrap_or(0);
            options.get(index).or_else(|| options.first())
        })
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "id": line_item_id,
        "status": "available",
        "price": price,
        "priceFactor": 100,
        "previousPrice": 0,
        "productId": product_id,
        "product": Value::Null,
        "instruction": "",
        "quantity": quantity,
        "selectedWeightRange": selected_weight_range,
        "specialInstruction": "",
        "storeId": str_at(product, "/storeId").unwrap_or_default(),
        "replacement": Value::Null,
        "replacementPreferenceId": "",
        "missionName": "",
        "missionType": "",
        "addToBasketType": "",
        "addToBasketJourney": "",
        "serviceOptionId": service_option_id,
        "isStockAvailable": bool_at(product, "/isStockAvailable").unwrap_or(true),
        "requiresOver18": bool_at(product, "/requiresOver18").unwrap_or(false),
        "isSponsoredProduct": bool_at(product, "/isSponsored").unwrap_or(false),
    }))
}

fn product_price_cents(product: &Value) -> Result<i64, String> {
    if let Some(price) = product.get("price").and_then(Value::as_f64) {
        return Ok((price * 100.0).round() as i64);
    }
    if let Some(price) = product.get("priceWithoutDecimal").and_then(Value::as_i64) {
        return Ok(price);
    }
    Err(format!("product has no price: {product}"))
}

fn cart_update_item(cart: &Value) -> Value {
    let mut line_items = cart
        .get("lineItems")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for line in &mut line_items {
        if let Some(map) = line.as_object_mut() {
            map.insert("product".to_string(), Value::Null);
        }
    }
    line_items.retain(|line| {
        line.get("quantity")
            .and_then(Value::as_u64)
            .is_some_and(|quantity| quantity > 0)
    });
    json!({
        "id": str_at(cart, "/id").unwrap_or_default(),
        "serviceOptionId": str_at(cart, "/serviceOptionId").unwrap_or_default(),
        "lineItems": line_items,
    })
}

fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn bool_at(v: &Value, pointer: &str) -> Option<bool> {
    v.pointer(pointer).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_bodies_match_browser_contract() {
        assert_eq!(
            verify_cell_body("+27712345678"),
            json!({ "mobileNumber": "+27712345678" })
        );
        assert_eq!(
            request_mobile_otp_body("+27712345678"),
            json!({ "mobileNumber": "+27712345678" })
        );
        assert_eq!(
            verify_otp_body("+27712345678", "123456", "otp-ref"),
            json!({
                "OTP": "123456",
                "OTPReference": "otp-ref",
                "mobileNumber": "+27712345678",
                "isEmail": false,
            })
        );
        assert_eq!(empty_body(), json!({}));

        let scheme = json!({ "scheme": "xtra", "uuid": "scheme-uuid" });
        assert_eq!(
            verify_date_of_birth_body("21/05/1990", &scheme),
            json!({ "dateOfBirth": "21/05/1990", "scheme": scheme })
        );
    }

    #[test]
    fn consent_and_identity_bodies_match_browser_contract() {
        assert_eq!(
            decrypt_scheme_body(&json!({ "uuid": "scheme-uuid" })).unwrap(),
            json!({ "hash": "{\"uuid\":\"scheme-uuid\"}" })
        );
        assert_eq!(
            sixty60_terms_consent_body("user-uuid"),
            json!({
                "data": {
                    "uuid": "user-uuid",
                    "consentPayload": [{
                        "consentTemplateId": "sixty60-za-termsandconditions",
                        "consentTemplateVersion": "1",
                        "granted": true,
                    }]
                }
            })
        );
        assert_eq!(
            xtra_savings_rewards_consent_body("user-uuid"),
            json!({
                "data": {
                    "uuid": "user-uuid",
                    "consentPayload": [{
                        "consentTemplateId": "checkers-za-rewards-consent",
                        "consentTemplateVersion": "1",
                        "granted": true,
                    }]
                }
            })
        );
        assert_eq!(
            validate_id_or_passport_body("9001015009086", "user-uid"),
            json!({ "idNumberOrPassportNumber": "9001015009086", "uid": "user-uid" })
        );
        assert_eq!(
            update_customer_id_or_passport_body(&json!({
                "passportNumber": "",
                "saIdNumber": "9001015009086",
                "birthDate": "1990/01/01",
            })),
            json!({
                "payloadObject": {
                    "passportNumber": "",
                    "saIdNumber": "9001015009086",
                    "birthDate": "1990/01/01",
                }
            })
        );
    }

    #[test]
    fn profile_bootstrap_bodies_match_browser_contract() {
        assert_eq!(
            get_user_profile_body(),
            json!({ "bypassHash": false, "isLogin": true })
        );
        assert_eq!(
            get_on_demand_profile_body("uid-123"),
            json!({ "uid": "uid-123" })
        );
        assert_eq!(
            get_user_addresses_body("address-123"),
            json!({ "addressId": "address-123" })
        );

        let address = json!({ "identifier": "address-123" });
        let profile = json!({
            "email": "person@example.com",
            "mobile": "+27712345678",
            "id": "user-id",
        });
        assert_eq!(
            fetch_store_contexts_body(&address, &profile),
            json!({
                "address": { "identifier": "address-123" },
                "email": "person@example.com",
                "mobileNumber": "+27712345678",
                "userId": "user-id",
                "update": true,
                "acceptedLimitedExperience": null,
            })
        );
    }

    #[test]
    fn shopping_bodies_match_store_contract() {
        assert_eq!(
            catalogue_search_body(&json!([{ "branchId": "store-1" }]), "milk"),
            json!({
                "storeContexts": [{ "branchId": "store-1" }],
                "filterData": {
                    "filter": {
                        "showAllDisplayVariants": false,
                        "showNotRangedProducts": false,
                        "productListSource": { "search": "milk" },
                        "paginationOptions": { "page": 0, "pageSize": 16 },
                        "filterOptions": {
                            "filterIds": [],
                            "dealsOnly": false,
                            "brandOptions": [],
                            "departmentOptions": [],
                            "serviceOptions": [],
                            "facetOptions": []
                        },
                        "sortOptions": null
                    },
                    "displayOptions": { "includeDisplayCategoryTree": false }
                }
            })
        );
        assert_eq!(
            catalogue_product_ids_body(&json!([{ "branchId": "store-1" }]), &["sku-123"]),
            json!({
                "storeContexts": [{ "branchId": "store-1" }],
                "filterData": {
                    "filter": {
                        "showAllDisplayVariants": true,
                        "showNotRangedProducts": true,
                        "productListSource": { "productIds": ["sku-123"] },
                        "paginationOptions": { "page": 0, "pageSize": 1 },
                        "filterOptions": {
                            "filterIds": [],
                            "dealsOnly": false,
                            "brandOptions": [],
                            "departmentOptions": [],
                            "serviceOptions": [],
                            "facetOptions": []
                        },
                        "sortOptions": null
                    },
                    "displayOptions": { "includeDisplayCategoryTree": false }
                }
            })
        );
        assert_eq!(
            fetch_cart_body(&json!([{ "branchId": "store-1" }])),
            json!({
                "params": {
                    "storeContexts": [{ "branchId": "store-1" }],
                    "serviceOptionIds": ["sixty-min-delivery", "one-day-delivery"]
                }
            })
        );
        assert_eq!(
            update_cart_payload_body(
                &json!({ "carts": [], "deliveryAddressId": "addr-1" }),
                false
            ),
            json!({
                "payload": { "carts": [], "deliveryAddressId": "addr-1" },
                "isNaiveUpdate": false
            })
        );
    }

    #[test]
    fn cart_fetch_response_unwraps_live_cart_items() {
        let response = json!({
            "carts": [
                {
                    "canBeMerged": true,
                    "item": {
                        "id": "cart-sixty",
                        "serviceOptionId": "sixty-min-delivery",
                        "lineItems": []
                    },
                    "replacementOptions": [],
                    "rollingDifference": 0
                },
                {
                    "item": {
                        "id": "cart-one-day",
                        "serviceOptionId": "one-day-delivery",
                        "lineItems": []
                    }
                }
            ]
        });

        assert_eq!(
            cart_items_from_fetch_response(&response),
            vec![
                json!({
                    "id": "cart-sixty",
                    "serviceOptionId": "sixty-min-delivery",
                    "lineItems": []
                }),
                json!({
                    "id": "cart-one-day",
                    "serviceOptionId": "one-day-delivery",
                    "lineItems": []
                })
            ]
        );
    }

    #[test]
    fn cart_response_product_presence_checks_live_line_items() {
        let response = json!({
            "carts": [{
                "item": {
                    "id": "cart-sixty",
                    "serviceOptionId": "sixty-min-delivery",
                    "lineItems": [{
                        "id": "line-1",
                        "productId": "product-123",
                        "quantity": 1
                    }]
                }
            }]
        });

        assert!(cart_response_contains_product(&response, "product-123"));
        assert!(!cart_response_contains_product(
            &response,
            "missing-product"
        ));
    }

    #[test]
    fn cart_update_payload_adds_new_product_with_browser_line_item_shape() {
        let carts = vec![json!({
            "id": "cart-sixty",
            "serviceOptionId": "sixty-min-delivery",
            "lineItems": [{
                "id": "existing-line",
                "productId": "existing-product",
                "product": { "name": "stripped before update" },
                "quantity": 1,
                "serviceOptionId": "sixty-min-delivery"
            }]
        })];
        let product = json!({
            "id": "product-123",
            "storeId": "store-1",
            "serviceOptionId": "sixty-min-delivery",
            "price": 64.99,
            "priceWithoutDecimal": 6499,
            "variableWeightOptions": null,
            "isStockAvailable": true,
            "requiresOver18": false,
            "isSponsored": false
        });

        let payload = build_cart_update_payload(
            &carts,
            &product,
            "sixty-min-delivery",
            2,
            "addr-1",
            &json!([{ "storeId": "store-1" }]),
            "line-product-123",
            None,
        )
        .unwrap();

        assert_eq!(payload["deliveryAddressId"], "addr-1");
        assert_eq!(payload["storeContexts"], json!([{ "storeId": "store-1" }]));
        assert_eq!(payload["carts"][0]["id"], "cart-sixty");
        assert_eq!(payload["carts"][0]["serviceOptionId"], "sixty-min-delivery");
        assert_eq!(
            payload["carts"][0]["lineItems"].as_array().unwrap().len(),
            2
        );
        assert_eq!(payload["carts"][0]["lineItems"][0]["product"], Value::Null);

        let added = &payload["carts"][0]["lineItems"][1];
        assert_eq!(added["id"], "line-product-123");
        assert_eq!(added["status"], "available");
        assert_eq!(added["productId"], "product-123");
        assert_eq!(added["product"], Value::Null);
        assert_eq!(added["quantity"], 2);
        assert_eq!(added["price"], 6499);
        assert_eq!(added["priceFactor"], 100);
        assert_eq!(added["selectedWeightRange"], Value::Null);
        assert_eq!(added["storeId"], "store-1");
        assert_eq!(added["serviceOptionId"], "sixty-min-delivery");
        assert_eq!(added["specialInstruction"], "");
    }

    #[test]
    fn cart_update_payload_increments_existing_product_quantity() {
        let carts = vec![json!({
            "id": "cart-sixty",
            "serviceOptionId": "sixty-min-delivery",
            "lineItems": [{
                "id": "existing-line",
                "productId": "product-123",
                "quantity": 1,
                "serviceOptionId": "sixty-min-delivery"
            }]
        })];
        let product = json!({
            "id": "product-123",
            "storeId": "store-1",
            "serviceOptionId": "sixty-min-delivery",
            "price": 64.99
        });

        let payload = build_cart_update_payload(
            &carts,
            &product,
            "sixty-min-delivery",
            3,
            "addr-1",
            &json!([]),
            "unused-new-line",
            None,
        )
        .unwrap();

        assert_eq!(
            payload["carts"][0]["lineItems"].as_array().unwrap().len(),
            1
        );
        assert_eq!(payload["carts"][0]["lineItems"][0]["id"], "existing-line");
        assert_eq!(payload["carts"][0]["lineItems"][0]["quantity"], 4);
        assert_eq!(
            payload["carts"][0]["lineItems"][0]["serviceOptionId"],
            "sixty-min-delivery"
        );
    }

    #[test]
    fn cart_update_payload_uses_selected_variable_weight_option() {
        let carts = vec![json!({
            "id": "cart-sixty",
            "serviceOptionId": "sixty-min-delivery",
            "lineItems": []
        })];
        let product = json!({
            "id": "product-123",
            "storeId": "store-1",
            "serviceOptionId": "sixty-min-delivery",
            "price": 99.99,
            "variableWeightOptions": [
                { "name": "300g", "minimumWeight": 300, "maximumWeight": 300 },
                { "name": "500g", "minimumWeight": 500, "maximumWeight": 500 }
            ]
        });

        let payload = build_cart_update_payload(
            &carts,
            &product,
            "sixty-min-delivery",
            3,
            "addr-1",
            &json!([]),
            "line-product-123",
            Some(1),
        )
        .unwrap();

        let added = &payload["carts"][0]["lineItems"][0];
        assert_eq!(added["quantity"], 3);
        assert_eq!(
            added["selectedWeightRange"],
            json!({ "name": "500g", "minimumWeight": 500, "maximumWeight": 500 })
        );
    }
}
