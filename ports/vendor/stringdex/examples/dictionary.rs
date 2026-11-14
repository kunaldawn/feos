use stringdex::internals::tree::encode_search_tree_ukkonen;
use stringdex::internals::{write_data_to_disk, write_tree_to_disk};

use std::io::{self, BufRead};
use std::{fs, mem};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if fs::exists("examples")? {
        std::env::set_current_dir("examples")?;
    } else if fs::exists("../examples")? {
        std::env::set_current_dir("../examples")?;
    }

    if fs::exists("search.index")? {
        fs::remove_dir_all("search.index")?;
    }
    if fs::exists("search.data")? {
        fs::remove_dir_all("search.data")?;
    }
    if fs::exists("search-index.js")? {
        fs::remove_file("search-index.js")?;
    }

    let mut serialized_root = Vec::new();

    let dataset = std::fs::read("../benches/words")?;
    let dataset_words: Vec<&[u8]> = dataset.split(|c: &u8| *c == b'\n').collect();
    let tree: stringdex::internals::tree::SearchTree =
        encode_search_tree_ukkonen(dataset_words.iter().map(|x| &x[..]));
    serialized_root.extend_from_slice(br#"rr_('{"word":{"I":""#);
    write_tree_to_disk(&tree, "search.index", &mut serialized_root)?;
    mem::drop(tree);
    serialized_root.extend_from_slice(br#"","#);
    serialized_root.extend(write_data_to_disk(
        &mut dataset_words.into_iter(),
        "search.data",
    )?);
    mem::drop(dataset);

    serialized_root.extend_from_slice(br#"},"definition":{"#);
    let dataset = io::BufReader::new(fs::File::open("../benches/definitions")?);
    let mut line_reading_error = None;
    serialized_root.extend(write_data_to_disk(
        &mut dataset.lines().map_while(|result| match result {
            Ok(definition) => Some(definition),
            Err(e) => {
                line_reading_error = Some(e);
                None
            }
        }),
        "search.data",
    )?);
    if let Some(error) = line_reading_error {
        return Err(error.into());
    }
    serialized_root.extend_from_slice(br#"}}')"#);
    fs::write("search-index.js", serialized_root)?;

    fs::copy("../js/stringdex.js", "stringdex.js")?;
    Ok(())
}
