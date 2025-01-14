use crossbeam_channel::Sender;
use femtovg::Color;
use std::any::Any;
use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use crate::anchor::Anchor;
use crate::error::FirewheelError;
use crate::event::{InputEvent, KeyboardEventsListen};
use crate::layer::{
    BackgroundLayer, StrongBackgroundLayerEntry, StrongLayerEntry, StrongWidgetLayerEntry,
    WeakRegionTreeEntry, WidgetLayer, WidgetLayerRef,
};
use crate::node::{
    BackgroundNodeRef, SetPointerLockType, StrongBackgroundNodeEntry, StrongWidgetNodeEntry,
    WidgetNode, WidgetNodeRef,
};
use crate::renderer::{BackgroundLayerRenderer, Renderer, WidgetLayerRenderer};
use crate::widget_node_set::WidgetNodeSet;
use crate::{
    BackgroundNode, ContainerRegionRef, EventCapturedStatus, PhysicalSize, Point, RegionInfo,
    ScaleFactor, Size, WidgetNodeRequests, VG,
};

pub struct AppWindow<A: Clone + Send + Sync + 'static> {
    pub(crate) layers_ordered: Vec<(i32, Vec<StrongLayerEntry<A>>)>,
    pub(crate) widget_layer_renderers_to_clean_up: Vec<WidgetLayerRenderer>,
    pub(crate) background_layer_renderers_to_clean_up: Vec<BackgroundLayerRenderer>,

    action_tx: Sender<A>,

    next_layer_id: u64,
    next_widget_id: u64,

    widget_with_pointer_lock: Option<(StrongWidgetNodeEntry<A>, SetPointerLockType)>,
    widgets_to_send_input_event: Vec<(StrongWidgetNodeEntry<A>, InputEvent)>,
    widget_with_text_comp_listen: Option<StrongWidgetNodeEntry<A>>,
    widgets_with_keyboard_listen: WidgetNodeSet<A>,
    widgets_scheduled_for_animation: WidgetNodeSet<A>,
    widgets_with_pointer_leave_listen: WidgetNodeSet<A>,
    widgets_to_remove_from_animation: Vec<StrongWidgetNodeEntry<A>>,
    widget_requests: Vec<(StrongWidgetNodeEntry<A>, WidgetNodeRequests)>,
    widgets_just_shown: WidgetNodeSet<A>,
    widgets_just_hidden: WidgetNodeSet<A>,

    renderer: Option<Renderer>,
    scale_factor: ScaleFactor,
    window_visibility: bool,

    do_repack_layers: bool,
}

impl<A: Clone + Send + Sync + 'static> AppWindow<A> {
    fn new(scale_factor: ScaleFactor, renderer: Renderer, action_tx: Sender<A>) -> Self {
        Self {
            next_layer_id: 0,
            next_widget_id: 0,
            layers_ordered: Vec::new(),
            widget_with_pointer_lock: None,
            widgets_to_send_input_event: Vec::new(),
            widget_with_text_comp_listen: None,
            widgets_with_keyboard_listen: WidgetNodeSet::new(),
            widgets_scheduled_for_animation: WidgetNodeSet::new(),
            widgets_with_pointer_leave_listen: WidgetNodeSet::new(),
            widgets_to_remove_from_animation: Vec::new(),
            widget_requests: Vec::new(),
            widgets_just_shown: WidgetNodeSet::new(),
            widgets_just_hidden: WidgetNodeSet::new(),
            widget_layer_renderers_to_clean_up: Vec::new(),
            background_layer_renderers_to_clean_up: Vec::new(),
            action_tx,
            renderer: Some(renderer),
            scale_factor,
            window_visibility: true,
            do_repack_layers: true,
        }
    }

    #[cfg(all(feature = "glutin", not(target_arch = "wasm32")))]
    pub fn new_from_glutin_display(
        scale_factor: ScaleFactor,
        display: &glutin::display::Display,
        action_tx: Sender<A>,
    ) -> Self {
        Self::new(
            scale_factor,
            Renderer::new_from_glutin_display(display),
            action_tx,
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub unsafe fn new_from_function<F>(
        scale_factor: ScaleFactor,
        load_fn: F,
        action_tx: Sender<A>,
    ) -> Self
    where
        F: FnMut(&str) -> *const c_void,
    {
        Self::new(
            scale_factor,
            Renderer::new_from_function(load_fn),
            action_tx,
        )
    }

    pub fn vg(&mut self) -> &mut VG {
        &mut self.renderer.as_mut().unwrap().vg
    }

    pub fn scale_factor(&self) -> ScaleFactor {
        self.scale_factor
    }

    pub fn add_widget_layer(
        &mut self,
        size: Size,
        z_order: i32,
        outer_position: Point,
        inner_position: Point,
        explicit_visibility: bool,
    ) -> WidgetLayerRef<A> {
        let new_id = self.next_layer_id;
        self.next_layer_id += 1;

        let layer_entry = StrongWidgetLayerEntry::new(WidgetLayer::new(
            new_id,
            z_order,
            size,
            outer_position,
            inner_position,
            explicit_visibility,
            self.window_visibility,
            self.scale_factor,
        ));

        let layer_ref = WidgetLayerRef {
            shared: layer_entry.downgrade(),
        };

        let layer_entry = StrongLayerEntry::Widget(layer_entry);

        let mut existing_z_order_i = None;
        let mut insert_i = 0;
        for (i, (z_order_2, _)) in self.layers_ordered.iter().enumerate() {
            if z_order == *z_order_2 {
                existing_z_order_i = Some(i);
                break;
            } else if z_order > *z_order_2 {
                insert_i = i + 1;
            }
        }
        if let Some(i) = existing_z_order_i {
            self.layers_ordered[i].1.push(layer_entry);
        } else {
            self.layers_ordered
                .insert(insert_i, (z_order, vec![layer_entry]));
        }

        self.do_repack_layers = true;

        layer_ref
    }

    pub fn remove_widget_layer(&mut self, layer: WidgetLayerRef<A>) -> Result<(), FirewheelError> {
        let (layer_id, layer_z_order) = if let Some(layer_entry) = layer.shared.upgrade() {
            let layer = layer_entry.borrow();

            if !layer.is_empty() {
                return Err(FirewheelError::LayerNotEmpty);
            }

            (layer.id, layer.z_order)
        } else {
            return Err(FirewheelError::LayerRemoved);
        };

        let mut remove_z_order_i = None;
        for (z_order_i, (z_order, layers)) in self.layers_ordered.iter_mut().enumerate() {
            if layer_z_order == *z_order {
                let mut remove_i = None;
                for (i, layer_entry) in layers.iter().enumerate() {
                    if let StrongLayerEntry::Widget(layer_entry) = layer_entry {
                        if layer_entry.borrow().id == layer_id {
                            remove_i = Some(i);
                            break;
                        }
                    }
                }
                if let Some(i) = remove_i {
                    let mut layer_entry = layers.remove(i);

                    if let StrongLayerEntry::Widget(layer_entry) = &mut layer_entry {
                        if let Some(renderer) = layer_entry.borrow_mut().renderer.take() {
                            self.widget_layer_renderers_to_clean_up.push(renderer);
                        }
                    }

                    if layers.is_empty() {
                        remove_z_order_i = Some(z_order_i);
                    }
                }

                break;
            }
        }
        if let Some(i) = remove_z_order_i {
            self.layers_ordered.remove(i);
        }

        self.do_repack_layers = true;

        Ok(())
    }

    pub fn set_widget_layer_outer_position(
        &mut self,
        layer: &mut WidgetLayerRef<A>,
        position: Point,
    ) -> Result<(), FirewheelError> {
        if let Some(mut layer_entry) = layer.shared.upgrade() {
            layer_entry
                .borrow_mut()
                .set_outer_position(position, self.scale_factor);
        } else {
            return Err(FirewheelError::LayerRemoved);
        }

        Ok(())
    }

    pub fn set_widget_layer_inner_position(
        &mut self,
        layer: &mut WidgetLayerRef<A>,
        position: Point,
    ) -> Result<(), FirewheelError> {
        if let Some(mut layer_entry) = layer.shared.upgrade() {
            layer_entry.borrow_mut().set_inner_position(
                position,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            );
        } else {
            return Err(FirewheelError::LayerRemoved);
        }

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn set_widget_layer_size(
        &mut self,
        layer: &mut WidgetLayerRef<A>,
        size: Size,
    ) -> Result<(), FirewheelError> {
        if let Some(mut layer_entry) = layer.shared.upgrade() {
            layer_entry.borrow_mut().set_size(
                size,
                self.scale_factor,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            );
        } else {
            return Err(FirewheelError::LayerRemoved);
        }

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn set_widget_layer_explicit_visibility(
        &mut self,
        layer: &mut WidgetLayerRef<A>,
        explicit_visibility: bool,
    ) -> Result<(), FirewheelError> {
        if let Some(mut layer_entry) = layer.shared.upgrade() {
            layer_entry.borrow_mut().set_explicit_visibility(
                explicit_visibility,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            );
        } else {
            return Err(FirewheelError::LayerRemoved);
        }

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn add_background_node(
        &mut self,
        size: Size,
        z_order: i32,
        outer_position: Point,
        explicit_visibility: bool,
        background_node: Box<dyn BackgroundNode>,
    ) -> BackgroundNodeRef {
        let new_id = self.next_layer_id;
        self.next_layer_id += 1;

        let mut node_entry = StrongBackgroundNodeEntry::new(background_node, new_id);

        let layer = BackgroundLayer::new(
            new_id,
            z_order,
            size,
            outer_position,
            explicit_visibility,
            self.window_visibility,
            self.scale_factor,
            node_entry.clone(),
        );

        let layer_entry = StrongBackgroundLayerEntry::new(layer);

        {
            node_entry.set_assigned_layer(layer_entry.downgrade());
        }

        let layer_entry = StrongLayerEntry::Background(layer_entry);

        let mut existing_z_order_i = None;
        let mut insert_i = 0;
        for (i, (z_order_2, _)) in self.layers_ordered.iter().enumerate() {
            if z_order == *z_order_2 {
                existing_z_order_i = Some(i);
                break;
            } else if z_order > *z_order_2 {
                insert_i = i + 1;
            }
        }
        if let Some(i) = existing_z_order_i {
            self.layers_ordered[i].1.push(layer_entry);
        } else {
            self.layers_ordered
                .insert(insert_i, (z_order, vec![layer_entry]));
        }

        self.do_repack_layers = true;

        BackgroundNodeRef {
            shared: node_entry.downgrade(),
        }
    }

    pub fn remove_background_node(
        &mut self,
        background_node: &mut BackgroundNodeRef,
    ) -> Result<(), FirewheelError> {
        let mut node_entry = background_node
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::BackgroundNodeRemoved)?;

        let layer_entry = node_entry.assigned_layer_mut().upgrade().unwrap();

        let (layer_id, layer_z_order) = {
            let layer = layer_entry.borrow();
            (layer.id, layer.z_order)
        };

        let mut remove_z_order_i = None;
        for (z_order_i, (z_order, layers)) in self.layers_ordered.iter_mut().enumerate() {
            if layer_z_order == *z_order {
                let mut remove_i = None;
                for (i, layer_entry) in layers.iter().enumerate() {
                    if let StrongLayerEntry::Background(layer_entry) = layer_entry {
                        if layer_entry.borrow().id == layer_id {
                            remove_i = Some(i);
                            break;
                        }
                    }
                }
                if let Some(i) = remove_i {
                    let mut layer_entry = layers.remove(i);

                    if let StrongLayerEntry::Background(layer_entry) = &mut layer_entry {
                        if let Some(renderer) = layer_entry.borrow_mut().renderer.take() {
                            self.background_layer_renderers_to_clean_up.push(renderer);
                        }
                    }

                    if layers.is_empty() {
                        remove_z_order_i = Some(z_order_i);
                    }
                }

                break;
            }
        }
        if let Some(i) = remove_z_order_i {
            self.layers_ordered.remove(i);
        }

        self.do_repack_layers = true;

        Ok(())
    }

    pub fn set_background_node_outer_position(
        &mut self,
        background_node: &mut BackgroundNodeRef,
        position: Point,
    ) -> Result<(), FirewheelError> {
        background_node
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::BackgroundNodeRemoved)?
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .set_outer_position(position, self.scale_factor);

        Ok(())
    }

    pub fn set_background_node_size(
        &mut self,
        background_node: &mut BackgroundNodeRef,
        size: Size,
    ) -> Result<(), FirewheelError> {
        background_node
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::BackgroundNodeRemoved)?
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .set_size(size, self.scale_factor);

        Ok(())
    }

    pub fn set_background_node_explicit_visibility(
        &mut self,
        background_node: &mut BackgroundNodeRef,
        explicit_visibility: bool,
    ) -> Result<(), FirewheelError> {
        background_node
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::BackgroundNodeRemoved)?
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .set_explicit_visibility(explicit_visibility);

        Ok(())
    }

    pub fn mark_background_node_dirty(
        &mut self,
        background_node: &mut BackgroundNodeRef,
    ) -> Result<(), FirewheelError> {
        background_node
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::BackgroundNodeRemoved)?
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .mark_dirty();

        Ok(())
    }

    pub fn send_user_event_to_background_node(
        &mut self,
        background_node: &mut BackgroundNodeRef,
        event: Box<dyn Any>,
    ) -> Result<(), FirewheelError> {
        let mut node_entry = background_node
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::BackgroundNodeRemoved)?;

        let mark_dirty = { node_entry.borrow_mut().on_user_event(event) };

        if mark_dirty {
            node_entry
                .assigned_layer_mut()
                .upgrade()
                .unwrap()
                .borrow_mut()
                .mark_dirty();
        }

        Ok(())
    }

    pub fn set_window_visibility(&mut self, visible: bool) {
        if self.window_visibility != visible {
            self.window_visibility = visible;

            for (_z_order, layers) in self.layers_ordered.iter_mut() {
                for layer_entry in layers.iter_mut() {
                    match layer_entry {
                        StrongLayerEntry::Widget(layer_entry) => {
                            layer_entry.borrow_mut().set_window_visibility(
                                visible,
                                &mut self.widgets_just_shown,
                                &mut self.widgets_just_hidden,
                            );
                        }
                        StrongLayerEntry::Background(layer_entry) => {
                            layer_entry.borrow_mut().set_window_visibility(visible);
                        }
                    }
                }
            }

            self.handle_visibility_changes();
        }
    }

    pub fn add_container_region(
        &mut self,
        layer: &WidgetLayerRef<A>,
        region_info: RegionInfo<A>,
        explicit_visibility: bool,
    ) -> Result<ContainerRegionRef<A>, FirewheelError> {
        if layer.shared.upgrade().is_none() {
            return Err(FirewheelError::LayerRemoved);
        }

        let weak_layer_entry = layer.shared.clone();

        weak_layer_entry
            .upgrade()
            .unwrap()
            .borrow_mut()
            .add_container_region(
                region_info,
                explicit_visibility,
                // No widgets will ever be shown or hidden as a result of
                // adding a container region.
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            )
            .map(|mut container_ref| {
                container_ref.assigned_layer = weak_layer_entry;
                container_ref
            })
    }

    pub fn remove_container_region(
        &mut self,
        region: ContainerRegionRef<A>,
    ) -> Result<(), FirewheelError> {
        region
            .assigned_layer
            .upgrade()
            .ok_or_else(|| FirewheelError::ContainerRegionRemoved)?
            .borrow_mut()
            .remove_container_region(region)
    }

    pub fn modify_container_region(
        &mut self,
        region: &mut ContainerRegionRef<A>,
        new_size: Option<Size>,
        new_internal_anchor: Option<Anchor>,
        new_parent_anchor: Option<Anchor>,
        new_anchor_offset: Option<Point>,
    ) -> Result<(), FirewheelError> {
        region
            .assigned_layer
            .upgrade()
            .ok_or_else(|| FirewheelError::ContainerRegionRemoved)?
            .borrow_mut()
            .modify_container_region(
                region,
                new_size,
                new_internal_anchor,
                new_parent_anchor,
                new_anchor_offset,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            )?;

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn set_container_region_explicit_visibility(
        &mut self,
        region: &mut ContainerRegionRef<A>,
        visible: bool,
    ) -> Result<(), FirewheelError> {
        region
            .assigned_layer
            .upgrade()
            .ok_or_else(|| FirewheelError::ContainerRegionRemoved)?
            .borrow_mut()
            .set_container_region_explicit_visibility(
                region,
                visible,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            )?;

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn mark_container_region_dirty(
        &mut self,
        region: &mut ContainerRegionRef<A>,
    ) -> Result<(), FirewheelError> {
        region
            .assigned_layer
            .upgrade()
            .ok_or_else(|| FirewheelError::ContainerRegionRemoved)?
            .borrow_mut()
            .mark_container_region_dirty(region)
    }

    pub fn add_widget_node(
        &mut self,
        mut widget_node: Box<dyn WidgetNode<A>>,
        layer: &WidgetLayerRef<A>,
        region_info: RegionInfo<A>,
        explicit_visibility: bool,
    ) -> Result<WidgetNodeRef<A>, FirewheelError> {
        if layer.shared.upgrade().is_none() {
            return Err(FirewheelError::LayerRemoved);
        }

        let weak_layer_entry = layer.shared.clone();

        let (node_type, requests) = widget_node.on_added(&mut self.action_tx);

        let new_id = self.next_widget_id;
        self.next_widget_id += 1;

        let mut widget_entry = StrongWidgetNodeEntry::new(
            Rc::new(RefCell::new(widget_node)),
            weak_layer_entry.clone(),
            WeakRegionTreeEntry::new(),
            new_id,
        );

        weak_layer_entry
            .upgrade()
            .unwrap()
            .borrow_mut()
            .add_widget_region(
                &mut widget_entry,
                region_info,
                node_type,
                explicit_visibility,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            )?;

        //self.widgets.insert(widget_entry.clone());

        self.handle_visibility_changes();

        self.handle_widget_requests(&mut widget_entry, requests);

        Ok(WidgetNodeRef {
            shared: widget_entry.downgrade(),
        })
    }

    pub fn modify_widget_region(
        &mut self,
        widget_node_ref: &mut WidgetNodeRef<A>,
        new_size: Option<Size>,
        new_internal_anchor: Option<Anchor>,
        new_parent_anchor: Option<Anchor>,
        new_anchor_offset: Option<Point>,
    ) -> Result<(), FirewheelError> {
        let mut widget_entry = widget_node_ref
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::WidgetNodeRemoved)?;

        widget_entry
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .modify_widget_region(
                &mut widget_entry,
                new_size,
                new_internal_anchor,
                new_parent_anchor,
                new_anchor_offset,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            );

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn set_widget_explicit_visibility(
        &mut self,
        widget_node_ref: &mut WidgetNodeRef<A>,
        visible: bool,
    ) -> Result<(), FirewheelError> {
        let mut widget_entry = widget_node_ref
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::WidgetNodeRemoved)?;

        widget_entry
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .set_widget_explicit_visibility(
                &mut widget_entry,
                visible,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            );

        self.handle_visibility_changes();

        Ok(())
    }

    pub fn remove_widget(
        &mut self,
        widget_node_ref: &mut WidgetNodeRef<A>,
    ) -> Result<(), FirewheelError> {
        let mut widget_entry = widget_node_ref
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::WidgetNodeRemoved)?;

        // Remove this widget from its assigned layer.
        widget_entry
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .remove_widget_region(
                &mut widget_entry,
                &mut self.widgets_just_shown,
                &mut self.widgets_just_hidden,
            );

        // Remove this widget from all active event listeners.
        self.widgets_scheduled_for_animation.remove(&widget_entry);
        self.widgets_with_keyboard_listen.remove(&widget_entry);
        self.widgets_with_pointer_leave_listen.remove(&widget_entry);
        if let Some(w) = self.widget_with_pointer_lock.take() {
            if w.0.unique_id() != widget_node_ref.unique_id() {
                self.widget_with_pointer_lock = Some(w);
            }
        }
        if let Some(w) = self.widget_with_text_comp_listen.take() {
            if w.unique_id() != widget_node_ref.unique_id() {
                self.widget_with_text_comp_listen = Some(w);
            }
        }

        Ok(())
    }

    pub fn send_user_event_to_widget(
        &mut self,
        widget_node_ref: &mut WidgetNodeRef<A>,
        event: Box<dyn Any>,
    ) -> Result<(), FirewheelError> {
        let mut widget_entry = widget_node_ref
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::WidgetNodeRemoved)?;

        let res = {
            widget_entry
                .borrow_mut()
                .on_user_event(event, &mut self.action_tx)
        };
        if let Some(requests) = res {
            self.handle_widget_requests(&mut widget_entry, requests);
        }

        Ok(())
    }

    pub fn mark_widget_dirty(
        &mut self,
        widget_node_ref: &mut WidgetNodeRef<A>,
    ) -> Result<(), FirewheelError> {
        let mut widget_entry = widget_node_ref
            .shared
            .upgrade()
            .ok_or_else(|| FirewheelError::WidgetNodeRemoved)?;

        widget_entry
            .assigned_layer_mut()
            .upgrade()
            .unwrap()
            .borrow_mut()
            .mark_widget_region_dirty(&widget_entry);

        Ok(())
    }

    pub fn set_scale_factor(&mut self, scale_factor: ScaleFactor) {
        if self.scale_factor != scale_factor {
            self.scale_factor = scale_factor;

            for (_z_order, layers) in self.layers_ordered.iter_mut() {
                for layer_entry in layers.iter_mut() {
                    match layer_entry {
                        StrongLayerEntry::Widget(layer_entry) => {
                            let mut layer_entry = layer_entry.borrow_mut();
                            let size = layer_entry.size();
                            layer_entry.set_size(
                                size,
                                scale_factor,
                                &mut self.widgets_just_shown,
                                &mut self.widgets_just_hidden,
                            );
                        }
                        StrongLayerEntry::Background(layer_entry) => {
                            let mut layer_entry = layer_entry.borrow_mut();
                            let size = layer_entry.size;
                            layer_entry.set_size(size, scale_factor);
                        }
                    }
                }
            }

            self.handle_visibility_changes();
        }
    }

    pub fn handle_input_event(&mut self, event: &InputEvent) -> InputEventResult {
        match event {
            InputEvent::Animation(_) => {
                let mut widgets_to_remove_from_animation: Vec<StrongWidgetNodeEntry<A>> =
                    Vec::new();
                let mut widget_requests: Vec<(StrongWidgetNodeEntry<A>, WidgetNodeRequests)> =
                    Vec::new();
                std::mem::swap(
                    &mut widgets_to_remove_from_animation,
                    &mut self.widgets_to_remove_from_animation,
                );
                std::mem::swap(&mut widget_requests, &mut self.widget_requests);

                for widget_entry in self.widgets_scheduled_for_animation.iter_mut() {
                    let res = {
                        widget_entry
                            .borrow_mut()
                            .on_input_event(event, &mut self.action_tx)
                    };
                    if let EventCapturedStatus::Captured(requests) = res {
                        widget_requests.push((widget_entry.clone(), requests));
                    } else {
                        widgets_to_remove_from_animation.push(widget_entry.clone());
                    }
                }

                for (mut widget_entry, requests) in widget_requests.drain(..) {
                    self.handle_widget_requests(&mut widget_entry, requests);
                }
                for widget_entry in widgets_to_remove_from_animation.drain(..) {
                    self.widgets_scheduled_for_animation.remove(&widget_entry);
                }

                std::mem::swap(
                    &mut widgets_to_remove_from_animation,
                    &mut self.widgets_to_remove_from_animation,
                );
                std::mem::swap(&mut widget_requests, &mut self.widget_requests);
            }
            InputEvent::Pointer(mut e) => {
                let pointer_locked_in_place = self
                    .widget_with_pointer_lock
                    .as_ref()
                    .map(|(_, lock_type)| {
                        *lock_type == SetPointerLockType::LockInPlaceAndHideCursor
                    })
                    .unwrap_or(false);

                if pointer_locked_in_place {
                    // Remove the position data when the pointer is locked in place.
                    e.position = Point::default();

                    let mut widget_entry =
                        self.widget_with_pointer_lock.as_ref().unwrap().0.clone();
                    let res = {
                        widget_entry
                            .borrow_mut()
                            .on_input_event(event, &mut self.action_tx)
                    };
                    if let EventCapturedStatus::Captured(requests) = res {
                        self.handle_widget_requests(&mut widget_entry, requests);
                    }
                } else {
                    if !self.widgets_with_pointer_leave_listen.is_empty() {
                        let mut widget_requests: Vec<(
                            StrongWidgetNodeEntry<A>,
                            WidgetNodeRequests,
                        )> = Vec::new();
                        std::mem::swap(&mut widget_requests, &mut self.widget_requests);

                        for widget_entry in self.widgets_with_pointer_leave_listen.iter_mut() {
                            let res = {
                                widget_entry
                                    .borrow_mut()
                                    .on_input_event(event, &mut self.action_tx)
                            };
                            if let EventCapturedStatus::Captured(requests) = res {
                                widget_requests.push((widget_entry.clone(), requests));
                            }
                        }

                        for (mut widget_entry, requests) in widget_requests.drain(..) {
                            self.handle_widget_requests(&mut widget_entry, requests);
                        }

                        std::mem::swap(&mut widget_requests, &mut self.widget_requests);
                    }

                    let mut widget_requests = None;
                    for (_z_index, layers) in self.layers_ordered.iter_mut().rev() {
                        for layer_entry in layers.iter_mut() {
                            if let StrongLayerEntry::Widget(layer_entry) = layer_entry {
                                if let Some(captured_res) = layer_entry
                                    .borrow_mut()
                                    .handle_pointer_event(e, &mut self.action_tx)
                                {
                                    widget_requests = Some(captured_res);
                                    break;
                                }
                            }
                        }
                        if widget_requests.is_some() {
                            break;
                        }
                    }

                    if let Some((mut widget_entry, requests)) = widget_requests {
                        self.handle_widget_requests(&mut widget_entry, requests);
                    }
                }
            }
            InputEvent::PointerUnlocked => {
                let mut requests = None;
                if let Some((mut last_widget, _lock_type)) = self.widget_with_pointer_lock.take() {
                    let res = {
                        last_widget
                            .borrow_mut()
                            .on_input_event(event, &mut self.action_tx)
                    };
                    if let EventCapturedStatus::Captured(r) = res {
                        requests = Some((last_widget.clone(), r));
                    }
                }

                if let Some((mut widget_entry, requests)) = requests {
                    self.handle_widget_requests(&mut widget_entry, requests);
                }
            }
            InputEvent::Keyboard(_) => {
                let mut widget_requests: Vec<(StrongWidgetNodeEntry<A>, WidgetNodeRequests)> =
                    Vec::new();
                std::mem::swap(&mut widget_requests, &mut self.widget_requests);

                for widget_entry in self.widgets_with_keyboard_listen.iter_mut() {
                    let res = {
                        widget_entry
                            .borrow_mut()
                            .on_input_event(event, &mut self.action_tx)
                    };
                    if let EventCapturedStatus::Captured(requests) = res {
                        widget_requests.push((widget_entry.clone(), requests));
                    }
                }

                for (mut widget_entry, requests) in widget_requests.drain(..) {
                    self.handle_widget_requests(&mut widget_entry, requests);
                }

                std::mem::swap(&mut widget_requests, &mut self.widget_requests);
            }
            InputEvent::TextComposition(_) => {
                let mut requests = None;
                if let Some(widget_entry) = &mut self.widget_with_text_comp_listen {
                    let res = {
                        widget_entry
                            .borrow_mut()
                            .on_input_event(event, &mut self.action_tx)
                    };
                    if let EventCapturedStatus::Captured(r) = res {
                        requests = Some((widget_entry.clone(), r));
                    }
                }

                if let Some((mut widget_entry, requests)) = requests {
                    self.handle_widget_requests(&mut widget_entry, requests);
                }
            }
            e => {
                log::warn!("Input event {:?} is reserved for internal use", e);
            }
        }

        // Handle any extra events that have occurred as a result of handling
        // widget requests.
        while let Some((mut widget_entry, event)) = self.widgets_to_send_input_event.pop() {
            let res = {
                widget_entry
                    .borrow_mut()
                    .on_input_event(&event, &mut self.action_tx)
            };
            if let EventCapturedStatus::Captured(requests) = res {
                self.handle_widget_requests(&mut widget_entry, requests);
            }
        }

        let lock_pointer_in_place = self
            .widget_with_pointer_lock
            .as_ref()
            .map(|(_, lock_type)| *lock_type == SetPointerLockType::LockInPlaceAndHideCursor)
            .unwrap_or(false);

        InputEventResult {
            lock_pointer_in_place,
        }
    }

    pub fn is_dirty(&self) -> bool {
        for (_z_order, layers) in self.layers_ordered.iter() {
            for layer_entry in layers.iter() {
                match layer_entry {
                    StrongLayerEntry::Widget(layer_entry) => {
                        if layer_entry.borrow().is_dirty() {
                            return true;
                        }
                    }
                    StrongLayerEntry::Background(layer_entry) => {
                        if layer_entry.borrow().is_dirty {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    pub fn render(&mut self, window_size: PhysicalSize, clear_color: Color) {
        let mut renderer = self.renderer.take().unwrap();

        renderer.render(self, window_size, self.scale_factor, clear_color);

        self.renderer = Some(renderer);
    }

    fn handle_widget_requests(
        &mut self,
        widget_entry: &mut StrongWidgetNodeEntry<A>,
        requests: WidgetNodeRequests,
    ) {
        if requests.repaint {
            // Note, the widget won't actually get marked dirty if it is
            // currently hidden.
            widget_entry
                .assigned_layer_mut()
                .upgrade()
                .unwrap()
                .borrow_mut()
                .mark_widget_region_dirty(widget_entry);
        }
        if let Some(recieve_next_animation_event) = requests.set_recieve_next_animation_event {
            if recieve_next_animation_event {
                let is_visible = {
                    widget_entry
                        .assigned_region()
                        .upgrade()
                        .unwrap()
                        .borrow()
                        .region
                        .is_visible()
                };
                if is_visible {
                    self.widgets_scheduled_for_animation.insert(widget_entry);
                }
            } else {
                self.widgets_scheduled_for_animation.remove(widget_entry);
            }
        }
        if let Some(listens) = requests.set_pointer_events_listen {
            widget_entry
                .assigned_layer_mut()
                .upgrade()
                .unwrap()
                .borrow_mut()
                .set_widget_region_listens_to_pointer_events(widget_entry, listens);
        }
        if let Some(set_keyboard_events_listen) = requests.set_keyboard_events_listen {
            let is_visible = {
                widget_entry
                    .assigned_region()
                    .upgrade()
                    .unwrap()
                    .borrow()
                    .region
                    .is_visible()
            };

            let set_text_comp = if is_visible {
                match set_keyboard_events_listen {
                    KeyboardEventsListen::None => {
                        self.widgets_with_keyboard_listen.remove(&widget_entry);
                        false
                    }
                    KeyboardEventsListen::Keys => {
                        self.widgets_with_keyboard_listen.insert(&widget_entry);
                        false
                    }
                    KeyboardEventsListen::TextComposition => {
                        self.widgets_with_keyboard_listen.remove(&widget_entry);
                        true
                    }
                    KeyboardEventsListen::KeysAndTextComposition => {
                        self.widgets_with_keyboard_listen.insert(&widget_entry);
                        true
                    }
                }
            } else {
                self.widgets_with_keyboard_listen.remove(&widget_entry);
                false
            };

            if set_text_comp {
                if let Some(last_widget) = self.widget_with_text_comp_listen.take() {
                    if last_widget.unique_id() != widget_entry.unique_id() {
                        self.widgets_to_send_input_event
                            .push((last_widget.clone(), InputEvent::TextCompositionUnfocused));
                        self.widgets_to_send_input_event
                            .push((widget_entry.clone(), InputEvent::TextCompositionFocused));

                        self.widget_with_text_comp_listen = Some(widget_entry.clone());
                    } else {
                        self.widget_with_text_comp_listen = Some(last_widget);
                    }
                } else {
                    self.widget_with_text_comp_listen = Some(widget_entry.clone());
                    self.widgets_to_send_input_event
                        .push((widget_entry.clone(), InputEvent::TextCompositionFocused));
                }
            } else {
                if let Some(last_widget) = self.widget_with_text_comp_listen.take() {
                    if last_widget.unique_id() == widget_entry.unique_id() {
                        self.widgets_to_send_input_event
                            .push((widget_entry.clone(), InputEvent::TextCompositionUnfocused));
                    } else {
                        self.widget_with_text_comp_listen = Some(last_widget);
                    }
                }
            }
        }
        if let Some(set_lock_type) = requests.set_pointer_lock {
            let is_visible = {
                widget_entry
                    .assigned_region()
                    .upgrade()
                    .unwrap()
                    .borrow()
                    .region
                    .is_visible()
            };

            if set_lock_type == SetPointerLockType::Unlock || !is_visible {
                if let Some((last_widget, lock_type)) = self.widget_with_pointer_lock.take() {
                    if last_widget.unique_id() == widget_entry.unique_id() {
                        self.widgets_to_send_input_event
                            .push((widget_entry.clone(), InputEvent::PointerUnlocked));
                    } else {
                        self.widget_with_pointer_lock = Some((last_widget, lock_type));
                    }
                }
            } else {
                if let Some((last_widget, _)) = &mut self.widget_with_pointer_lock {
                    if last_widget.unique_id() != widget_entry.unique_id() {
                        self.widgets_to_send_input_event
                            .push((last_widget.clone(), InputEvent::PointerUnlocked));
                    } else {
                        self.widget_with_pointer_lock = Some((widget_entry.clone(), set_lock_type));
                    }
                } else {
                    self.widget_with_pointer_lock = Some((widget_entry.clone(), set_lock_type));
                    self.widgets_to_send_input_event
                        .push((widget_entry.clone(), InputEvent::PointerLocked));
                }
            }
        }
        if let Some(set_pointer_leave_listen) = requests.set_pointer_leave_listen {
            let is_visible = {
                widget_entry
                    .assigned_region()
                    .upgrade()
                    .unwrap()
                    .borrow()
                    .region
                    .is_visible()
            };

            if set_pointer_leave_listen && is_visible {
                self.widgets_with_pointer_leave_listen.insert(&widget_entry);
            } else {
                self.widgets_with_pointer_leave_listen.remove(&widget_entry);
            }
        }
    }

    fn handle_visibility_changes(&mut self) {
        // Handle widgets that have just been shown.
        while let Some(mut widget_entry) = self.widgets_just_shown.pop() {
            let status = {
                widget_entry
                    .borrow_mut()
                    .on_input_event(&InputEvent::VisibilityShown, &mut self.action_tx)
            };
            if let EventCapturedStatus::Captured(requests) = status {
                self.handle_widget_requests(&mut widget_entry, requests);
            }
        }
        self.widgets_just_shown.clear();

        // Handle widgets that have just been hidden.
        while let Some(mut widget_entry) = self.widgets_just_hidden.pop() {
            {
                widget_entry
                    .borrow_mut()
                    .on_visibility_hidden(&mut self.action_tx);
            }

            // Remove all event listeners for this widget (except for pointer
            // input events, because the region tree already culls pointer
            // input events from hidden widgets).
            self.widgets_scheduled_for_animation.remove(&widget_entry);
            self.widgets_with_keyboard_listen.remove(&widget_entry);
            self.widgets_with_pointer_leave_listen.remove(&widget_entry);
            if let Some((last_widget, lock_type)) = self.widget_with_pointer_lock.take() {
                if last_widget.unique_id() != widget_entry.unique_id() {
                    self.widget_with_pointer_lock = Some((last_widget, lock_type));
                }
            }
            if let Some(last_widget) = self.widget_with_text_comp_listen.take() {
                if last_widget.unique_id() != widget_entry.unique_id() {
                    self.widget_with_text_comp_listen = Some(last_widget);
                }
            }
        }
        self.widgets_just_hidden.clear();
    }
}

impl<A: Clone + Send + Sync + 'static> Drop for AppWindow<A> {
    fn drop(&mut self) {
        for (_z_order, layers) in self.layers_ordered.iter_mut() {
            for layer_entry in layers.iter_mut() {
                match layer_entry {
                    StrongLayerEntry::Widget(layer_entry) => {
                        if let Some(renderer) = layer_entry.borrow_mut().renderer.take() {
                            self.widget_layer_renderers_to_clean_up.push(renderer);
                        }
                    }
                    StrongLayerEntry::Background(layer_entry) => {
                        if let Some(renderer) = layer_entry.borrow_mut().renderer.take() {
                            self.background_layer_renderers_to_clean_up.push(renderer);
                        }
                    }
                }
            }
        }

        let mut renderer = self.renderer.take().unwrap();

        renderer.free(self);
    }
}

pub struct InputEventResult {
    pub lock_pointer_in_place: bool,
    // TODO: cursor icon
}
