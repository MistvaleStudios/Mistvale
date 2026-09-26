//! The items players have: for now, a fixed hotbar of building blocks.
//!
//! Items need network IDs registered in the ItemRegistry sent with StartGame.
//! Mistvale registers only the items it hands out, with vanilla's IDs and
//! entry versions (as in PocketMine's `required_item_list.json`), and with no
//! block states, since their block states are just their names.

use mistvale_protocol::block::BlockState;
use mistvale_protocol::nbt::Compound;
use mistvale_protocol::packets::{
    INVENTORY_WINDOW, InventoryContent, ItemEntry, ItemInstance, ItemRegistry,
};

/// Slots in the player's inventory window, hotbar first.
const INVENTORY_SLOTS: usize = 36;

/// A block every player gets a stack of, with its vanilla item network ID.
struct HotbarBlock {
    name: &'static str,
    item_id: i16,
}

/// The hotbar, slot 0 first.
const HOTBAR: [HotbarBlock; 9] = [
    HotbarBlock {
        name: "minecraft:stone",
        item_id: 1,
    },
    HotbarBlock {
        name: "minecraft:grass_block",
        item_id: 2,
    },
    HotbarBlock {
        name: "minecraft:dirt",
        item_id: 3,
    },
    HotbarBlock {
        name: "minecraft:cobblestone",
        item_id: 4,
    },
    HotbarBlock {
        name: "minecraft:oak_planks",
        item_id: 5,
    },
    HotbarBlock {
        name: "minecraft:sand",
        item_id: 12,
    },
    HotbarBlock {
        name: "minecraft:glass",
        item_id: 20,
    },
    HotbarBlock {
        name: "minecraft:white_wool",
        item_id: 35,
    },
    HotbarBlock {
        name: "minecraft:bookshelf",
        item_id: 47,
    },
];

/// The items the client needs to know: those on the hotbar.
pub fn item_registry() -> ItemRegistry {
    ItemRegistry {
        items: HOTBAR
            .iter()
            .map(|block| ItemEntry {
                name: block.name.into(),
                runtime_id: block.item_id,
                component_based: false,
                // Vanilla items are version 2 ("none").
                version: 2,
                data: Compound::new(),
            })
            .collect(),
    }
}

/// The inventory window every player starts with: a stack of each hotbar
/// block, and the rest empty.
pub fn starting_inventory() -> InventoryContent {
    let mut content = vec![ItemInstance::EMPTY; INVENTORY_SLOTS];
    for (slot, block) in HOTBAR.iter().enumerate() {
        content[slot] = ItemInstance {
            network_id: block.item_id,
            count: 64,
            metadata: 0,
            // Stack IDs only need to be unique per player.
            stack_network_id: Some(slot as i32 + 1),
            block_runtime_id: BlockState::new(block.name).network_id(),
        };
    }
    InventoryContent {
        window_id: INVENTORY_WINDOW,
        content,
    }
}

/// The block the hotbar slot places, as a network ID. The inventory is fixed
/// and creative, so stacks never run out.
pub fn hotbar_block(slot: i32) -> Option<u32> {
    let block = HOTBAR.get(usize::try_from(slot).ok()?)?;
    Some(BlockState::new(block.name).network_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hotbar_holds_registered_blocks() {
        let registry = item_registry();
        let inventory = starting_inventory();
        assert_eq!(inventory.content.len(), 36);
        for (slot, item) in inventory.content[..9].iter().enumerate() {
            assert!(
                registry
                    .items
                    .iter()
                    .any(|entry| entry.runtime_id == item.network_id)
            );
            assert_eq!(Some(item.block_runtime_id), hotbar_block(slot as i32));
        }
        assert!(inventory.content[9..].iter().all(ItemInstance::is_empty));
        assert_eq!(hotbar_block(9), None);
        assert_eq!(hotbar_block(-1), None);
    }
}
