use crate::{
    core::{
        algebra::Matrix4, arrayvec::ArrayVec, parking_lot::Mutex, pool::Handle, scope_profile,
        sstorage::ImmutableString,
    },
    material::{Material, PropertyValue},
    scene::{
        graph::Graph,
        mesh::{surface::SurfaceData, RenderPath},
        node::Node,
    },
    utils::log::{Log, MessageKind},
};
use fxhash::{FxHashMap, FxHasher};
use std::{
    fmt::{Debug, Formatter},
    hash::Hasher,
    sync::Arc,
};

pub const BONE_MATRICES_COUNT: usize = 64;

pub struct SurfaceInstance {
    pub owner: Handle<Node>,
    pub world_transform: Matrix4<f32>,
    pub bone_matrices: ArrayVec<Matrix4<f32>, BONE_MATRICES_COUNT>,
    pub depth_offset: f32,
}

pub struct Batch {
    pub data: Arc<Mutex<SurfaceData>>,
    pub instances: Vec<SurfaceInstance>,
    pub material: Arc<Mutex<Material>>,
    pub is_skinned: bool,
    pub render_path: RenderPath,
    pub decal_layer_index: u8,
    sort_index: u64,
}

impl Debug for Batch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Batch {}: {} instances",
            &*self.data as *const _ as u64,
            self.instances.len()
        )
    }
}

#[derive(Default)]
pub struct BatchStorage {
    buffers: Vec<Vec<SurfaceInstance>>,
    batch_map: FxHashMap<u64, usize>,
    /// Sorted list of batches.
    pub batches: Vec<Batch>,
}

impl BatchStorage {
    pub(in crate) fn generate_batches(&mut self, graph: &Graph) {
        scope_profile!();

        for batch in self.batches.iter_mut() {
            batch.instances.clear();
            self.buffers.push(std::mem::take(&mut batch.instances));
        }

        self.batches.clear();
        self.batch_map.clear();

        for (handle, node) in graph.pair_iter() {
            match node {
                Node::Mesh(mesh) => {
                    for surface in mesh.surfaces().iter() {
                        let is_skinned = !surface.bones.is_empty();

                        let world = if is_skinned {
                            Matrix4::identity()
                        } else {
                            mesh.global_transform()
                        };

                        let data = surface.data();
                        let batch_id = surface.batch_id();

                        let batch = if let Some(&batch_index) = self.batch_map.get(&batch_id) {
                            self.batches.get_mut(batch_index).unwrap()
                        } else {
                            self.batch_map.insert(batch_id, self.batches.len());
                            self.batches.push(Batch {
                                data,
                                // Batches from meshes will be sorted using materials.
                                // This will significantly reduce pipeline state changes.
                                sort_index: surface.material_id(),
                                instances: self.buffers.pop().unwrap_or_default(),
                                material: surface.material().clone(),
                                is_skinned: !surface.bones.is_empty(),
                                render_path: mesh.render_path(),
                                decal_layer_index: mesh.decal_layer_index(),
                            });
                            self.batches.last_mut().unwrap()
                        };

                        batch.sort_index = surface.material_id();
                        batch.material = surface.material().clone();

                        batch.instances.push(SurfaceInstance {
                            world_transform: world,
                            bone_matrices: surface
                                .bones
                                .iter()
                                .map(|&bone_handle| {
                                    let bone_node = &graph[bone_handle];
                                    bone_node.global_transform()
                                        * bone_node.inv_bind_pose_transform()
                                })
                                .collect(),
                            owner: handle,
                            depth_offset: mesh.depth_offset_factor(),
                        });
                    }
                }
                Node::Terrain(terrain) => {
                    for (layer_index, layer) in terrain.layers().iter().enumerate() {
                        for (chunk_index, chunk) in terrain.chunks_ref().iter().enumerate() {
                            let data = chunk.data();
                            let data_key = &*data as *const _ as u64;

                            let mut material = (*layer.material.lock()).clone();
                            match material.set_property(
                                &ImmutableString::new(&layer.mask_property_name),
                                PropertyValue::Sampler {
                                    value: Some(layer.chunk_masks[chunk_index].clone()),
                                    fallback: Default::default(),
                                },
                            ) {
                                Ok(_) => {
                                    let material = Arc::new(Mutex::new(material));

                                    let mut hasher = FxHasher::default();

                                    hasher.write_u64(&*material as *const _ as u64);
                                    hasher.write_u64(data_key);

                                    let key = hasher.finish();

                                    let batch = if let Some(&batch_index) = self.batch_map.get(&key)
                                    {
                                        self.batches.get_mut(batch_index).unwrap()
                                    } else {
                                        self.batch_map.insert(key, self.batches.len());
                                        self.batches.push(Batch {
                                            data: data.clone(),
                                            instances: self.buffers.pop().unwrap_or_default(),
                                            material: material.clone(),
                                            is_skinned: false,
                                            render_path: RenderPath::Deferred,
                                            sort_index: layer_index as u64,
                                            decal_layer_index: terrain.decal_layer_index(),
                                        });
                                        self.batches.last_mut().unwrap()
                                    };

                                    batch.sort_index = layer_index as u64;
                                    batch.material = material;

                                    batch.instances.push(SurfaceInstance {
                                        world_transform: terrain.global_transform(),
                                        bone_matrices: Default::default(),
                                        owner: handle,
                                        depth_offset: terrain.depth_offset_factor(),
                                    });
                                }
                                Err(e) => Log::writeln(
                                    MessageKind::Error,
                                    format!(
                                        "Failed to prepare batch for terrain chunk.\
                                 Unable to set mask texture for terrain material. Reason: {:?}",
                                        e
                                    ),
                                ),
                            }
                        }
                    }
                }
                _ => (),
            }
        }

        for batch in self.batches.iter_mut() {
            batch.instances.shrink_to_fit();
        }

        self.batches.sort_unstable_by_key(|b| b.sort_index);
    }
}
