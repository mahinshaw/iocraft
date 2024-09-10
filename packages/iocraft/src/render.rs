use crate::{
    canvas::{Canvas, CanvasSubviewMut},
    component::{ComponentContextProvider, ComponentHelperExt, Components, InstantiatedComponent},
    context::{ExitMode, SystemContext},
    element::ElementExt,
    props::AnyProps,
};
use crossterm::{cursor, queue, terminal};
use std::{
    any::Any,
    collections::HashMap,
    io::{self, stdout, Write},
    mem,
};
pub use taffy::NodeId;
use taffy::{AvailableSpace, Layout, Point, Size, Style, TaffyTree};

pub struct ComponentUpdater<'a> {
    node_id: NodeId,
    children: &'a mut Components,
    layout_engine: &'a mut LayoutEngine,
    context_provider: &'a ComponentContextProvider<'a>,
}

impl<'a> ComponentUpdater<'a> {
    pub(crate) fn new(
        node_id: NodeId,
        children: &'a mut Components,
        layout_engine: &'a mut LayoutEngine,
        context_provider: &'a ComponentContextProvider<'a>,
    ) -> Self {
        Self {
            node_id,
            children,
            layout_engine,
            context_provider,
        }
    }

    pub fn get_context<T: Any>(&self) -> Option<&T> {
        self.context_provider.get_context()
    }

    pub fn set_layout_style(&mut self, layout_style: taffy::style::Style) {
        self.layout_engine
            .set_style(self.node_id, layout_style)
            .expect("we should be able to set the style");
    }

    pub fn set_measure_func(&mut self, measure_func: MeasureFunc) {
        self.layout_engine
            .get_node_context_mut(self.node_id)
            .expect("we should be able to get the node")
            .measure_func = Some(measure_func);
        self.layout_engine
            .mark_dirty(self.node_id)
            .expect("we should be able to mark the node as dirty");
    }

    pub fn update_children<I, T>(&mut self, children: I, context: Option<Box<&dyn Any>>)
    where
        I: IntoIterator<Item = T>,
        T: ElementExt,
    {
        let context_provider = context.map(|cx| self.context_provider.with_context(cx));
        let context_provider = context_provider.as_ref().unwrap_or(self.context_provider);
        let mut used_components = HashMap::with_capacity(self.children.components.len());

        for child in children {
            let mut component: InstantiatedComponent = match self
                .children
                .components
                .remove(child.key())
            {
                Some(component)
                    if component.component().type_id() == child.helper().component_type_id() =>
                {
                    component
                }
                _ => {
                    let new_node_id = self
                        .layout_engine
                        .new_leaf_with_context(Style::default(), LayoutEngineNodeContext::default())
                        .expect("we should be able to add the node");
                    self.layout_engine
                        .add_child(self.node_id, new_node_id)
                        .expect("we should be able to add the child");
                    InstantiatedComponent::new(new_node_id, child.props(), child.helper())
                }
            };
            component.update(self.layout_engine, &context_provider, child.props());
            if used_components
                .insert(child.key().clone(), component)
                .is_some()
            {
                panic!("duplicate key for sibling components: {}", child.key());
            }
        }

        for (_, component) in self.children.components.drain() {
            self.layout_engine
                .remove(component.node_id())
                .expect("we should be able to remove the node");
        }
        mem::swap(&mut self.children.components, &mut used_components);
    }
}

struct RenderContext<'a> {
    layout_engine: &'a LayoutEngine,
    canvas: &'a mut Canvas,
}

pub struct ComponentRenderer<'a> {
    node_id: NodeId,
    node_position: Point<u16>,
    node_size: Size<u16>,
    context: RenderContext<'a>,
}

impl<'a> ComponentRenderer<'a> {
    /// Gets the calculated layout of the current node.
    pub fn layout(&self) -> Layout {
        self.context
            .layout_engine
            .layout(self.node_id)
            .expect("we should be able to get the layout")
            .clone()
    }

    pub fn canvas(&mut self) -> CanvasSubviewMut {
        self.context.canvas.subview_mut(
            self.node_position.x as usize,
            self.node_position.y as usize,
            self.node_size.width as usize,
            self.node_size.height as usize,
            true,
        )
    }

    /// Prepares to begin rendering a node by moving to the node's position and invoking the given
    /// closure.
    pub(crate) fn for_child_node<F>(&mut self, node_id: NodeId, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let old_node_id = self.node_id;
        let old_node_position = self.node_position;
        let old_node_size = self.node_size;
        self.node_id = node_id;
        let layout = self.layout();
        self.node_position = Point {
            x: self.node_position.x + layout.location.x as u16,
            y: self.node_position.y + layout.location.y as u16,
        };
        self.node_size = Size {
            width: layout.size.width as u16,
            height: layout.size.height as u16,
        };
        f(self);
        self.node_id = old_node_id;
        self.node_position = old_node_position;
        self.node_size = old_node_size;
    }
}

type MeasureFunc = Box<dyn Fn(Size<Option<f32>>, Size<AvailableSpace>, &Style) -> Size<f32>>;

#[derive(Default)]
pub(crate) struct LayoutEngineNodeContext {
    measure_func: Option<MeasureFunc>,
}

pub(crate) type LayoutEngine = TaffyTree<LayoutEngineNodeContext>;

struct Tree<'a> {
    layout_engine: LayoutEngine,
    wrapper_node_id: NodeId,
    root_component: InstantiatedComponent,
    root_component_props: AnyProps<'a>,
    system_context: SystemContext,
}

impl<'a> Tree<'a> {
    fn new(props: AnyProps<'a>, helper: Box<dyn ComponentHelperExt>) -> Self {
        let mut layout_engine = TaffyTree::new();
        let root_node_id = layout_engine
            .new_leaf_with_context(Style::default(), LayoutEngineNodeContext::default())
            .expect("we should be able to add the root");
        let wrapper_node_id = layout_engine
            .new_with_children(Style::default(), &[root_node_id])
            .expect("we should be able to add the root");
        Self {
            layout_engine,
            wrapper_node_id,
            root_component: InstantiatedComponent::new(root_node_id, props.borrow(), helper),
            root_component_props: props,
            system_context: SystemContext::new(),
        }
    }

    fn render(&mut self, max_width: Option<usize>) -> Canvas {
        let context = ComponentContextProvider::root(Box::new(&self.system_context));
        self.root_component.update(
            &mut self.layout_engine,
            &context,
            self.root_component_props.borrow(),
        );

        self.layout_engine
            .compute_layout_with_measure(
                self.wrapper_node_id,
                Size {
                    width: max_width
                        .map(|w| AvailableSpace::Definite(w as _))
                        .unwrap_or(AvailableSpace::MaxContent),
                    height: AvailableSpace::MaxContent,
                },
                |known_dimensions, available_space, _node_id, node_context, style| {
                    match node_context.and_then(|cx| cx.measure_func.as_ref()) {
                        Some(f) => f(known_dimensions, available_space, style),
                        None => Size::ZERO,
                    }
                },
            )
            .expect("we should be able to compute the layout");

        let wrapper_layout = self
            .layout_engine
            .layout(self.wrapper_node_id)
            .expect("we should be able to get the wrapper layout");
        let mut canvas = Canvas::new(
            wrapper_layout.size.width as _,
            wrapper_layout.size.height as _,
        );
        let root_layout = self
            .layout_engine
            .layout(self.root_component.node_id())
            .expect("we should be able to get the root layout");
        let mut renderer = ComponentRenderer {
            node_id: self.root_component.node_id(),
            node_position: Point {
                x: root_layout.location.x as _,
                y: root_layout.location.y as _,
            },
            node_size: Size {
                width: root_layout.size.width as _,
                height: root_layout.size.height as _,
            },
            context: RenderContext {
                layout_engine: &self.layout_engine,
                canvas: &mut canvas,
            },
        };
        self.root_component.render(&mut renderer);
        canvas
    }

    async fn terminal_render_loop(&mut self) -> io::Result<()> {
        let mut dest = stdout();
        queue!(dest, cursor::SavePosition)?;
        loop {
            let (width, _) = terminal::size()?;
            queue!(
                dest,
                terminal::BeginSynchronizedUpdate,
                cursor::RestorePosition,
                terminal::Clear(terminal::ClearType::FromCursorDown),
            )?;
            dest.flush()?;
            let canvas = self.render(Some(width as _));
            queue!(dest, cursor::SavePosition, terminal::EndSynchronizedUpdate)?;
            if self.system_context.exit_mode() == Some(ExitMode::ClearOutput) {
                break;
            }
            // TODO: by comparing this canvas to the previous one, we could do incremental updates
            // instead of redrawing everything
            canvas.write_ansi(stdout())?;
            if self.system_context.exit_mode().is_some() {
                break;
            }
            self.root_component.wait().await;
        }
        Ok(())
    }
}

pub fn render<E: ElementExt>(e: E, max_width: Option<usize>) -> Canvas {
    let mut tree = Tree::new(e.props(), e.helper());
    tree.render(max_width)
}

pub(crate) async fn terminal_render_loop<E: ElementExt>(e: E) -> io::Result<()> {
    let mut tree = Tree::new(e.props(), e.helper());
    tree.terminal_render_loop().await
}
