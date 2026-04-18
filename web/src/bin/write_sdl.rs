fn main() {
    use async_graphql::SDLExportOptions;

    println!(
        "{}",
        galoy_agents_web::graphql::schema(None)
            .sdl_with_options(
                SDLExportOptions::new()
                    .sorted_fields()
                    .sorted_arguments()
                    .sorted_enum_items(),
            )
            .trim()
    );
}
