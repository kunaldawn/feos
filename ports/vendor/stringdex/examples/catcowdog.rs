use stringdex::internals::tree::encode_search_tree_ukkonen;
use stringdex::internals::{write_data_to_disk, write_tree_to_disk};

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

    let dataset_words = [b"cat", b"cow", b"dog"];
    let dataset_definitions = [b"a feline", b"a bovine", b"a canine"];
    let tree: stringdex::internals::tree::SearchTree =
        encode_search_tree_ukkonen(dataset_words.iter().map(|x| &x[..]));
    serialized_root.extend_from_slice(br#"rr_('{"word":{"I":""#);
    write_tree_to_disk(&tree, "search.index", &mut serialized_root)?;
    mem::drop(tree);
    serialized_root.extend_from_slice(br#"","#);
    serialized_root.extend(write_data_to_disk(
        &mut dataset_words.iter(),
        "search.data",
    )?);

    serialized_root.extend_from_slice(br#"},"definition":{"#);
    serialized_root.extend(write_data_to_disk(
        &mut dataset_definitions.iter(),
        "search.data",
    )?);
    serialized_root.extend_from_slice(br#"}}')"#);
    fs::write("search-index.js", serialized_root)?;

    fs::copy("../js/stringdex.js", "stringdex.js")?;
    Ok(())
}
