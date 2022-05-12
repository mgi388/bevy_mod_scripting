pub mod assets;

use crate::{
    script_add_synchronizer, script_hot_reload_handler, script_remove_synchronizer, APIProvider,
    CachedScriptEventState, ScriptContexts, ScriptHost,
};
use anyhow::{anyhow, Result};
use beau_collector::BeauCollector as _;
use bevy::prelude::*;
use bevy_event_priority::AddPriorityEvent;
use rlua::prelude::*;
use rlua::{Context, Function, Lua, MultiValue, ToLua, ToLuaMulti};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::Mutex;

pub use assets::*;

// /// defines a value allowed to be passed as lua script arguments for callbacks
// /// TODO: expand this
// #[derive(Clone)]
// pub enum LuaCallbackArgument {
//     Integer(usize),
// }

// impl<'lua> ToLua<'lua> for LuaCallbackArgument {
//     fn to_lua(self, lua: Context<'lua>) -> LuaResult<LuaValue<'lua>> {
//         match self {
//             LuaCallbackArgument::Integer(i) => i.to_lua(lua),
//         }
//     }
// }

pub trait LuaArg: for<'lua> ToLua<'lua> + Clone + Sync + Send + 'static {}

impl<T: for<'lua> ToLua<'lua> + Clone + Sync + Send + 'static> LuaArg for T {}

#[derive(Clone)]
/// A Lua Hook. The result of creating this event will be
/// a call to the lua script with the hook_name and the given arguments
pub struct LuaEvent<A: LuaArg> {
    pub hook_name: String,
    pub args: Vec<A>,
}

/// Rlua script host, enables Lua scripting provided by the Rlua library.
/// Always provides two global variables to each script by default:
///     - `world` - a raw pointer to the `bevy::World` the script lives in
///     - `entity` - an `Entity::to_bits` representation of the entity the script is attached to
///
/// # Examples
///
/// You can use these variables in your APIProviders like so:
/// ```
///    use std::sync::Mutex;
///    use bevy::prelude::*;
///    use rlua::prelude::*;
///    use bevy_scripting::{RLuaScriptHost, APIProvider};
///    
///    #[derive(Default)]
///    pub struct LuaAPIProvider {}
///
///    #[derive(Clone)]
///    pub struct MyLuaArg;
///
///    impl<'lua> ToLua<'lua> for MyLuaArg {
///        fn to_lua(self, _lua: rlua::Context<'lua>) -> rlua::Result<rlua::Value<'lua>> {
///            Ok(rlua::Value::Nil)
///        }
///    }

///    /// the custom Lua api, world is provided via a global pointer,
///    /// and callbacks are defined only once at script creation
///    impl APIProvider for LuaAPIProvider {
///        type Ctx = Mutex<Lua>;
///        fn attach_api(ctx: &mut Self::Ctx) {
///            // callbacks can receive any `ToLuaMulti` arguments, here '()' and
///            // return any `FromLuaMulti` arguments, here a `usize`
///            // check the Rlua documentation for more details
///            RLuaScriptHost::<MyLuaArg,Self>::register_api_callback(
///                "your_callback",
///                |ctx, ()| {
///                    let globals = ctx.globals();
///
///                    // retrieve the world pointer
///                    let world_data: LuaLightUserData = globals.get("world").unwrap();
///                    let world = unsafe { &mut *(world_data.0 as *mut World) };
///                    
///                    // retrieve script entity
///                    let entity_id : u64 = globals.get("entity").unwrap();
///                    let entity : Entity = Entity::from_bits(entity_id);
///
///                    
///                    Ok(())
///                },
///                ctx,
///            )
///        }
///    }
/// ```
#[derive(Default)]
pub struct RLuaScriptHost<A: LuaArg, API: APIProvider> {
    _ph: PhantomData<API>,
    _ph2: PhantomData<A>,
}

unsafe impl<A: LuaArg, API: APIProvider> Send for RLuaScriptHost<A, API> {}
unsafe impl<A: LuaArg, API: APIProvider> Sync for RLuaScriptHost<A, API> {}

impl<A: LuaArg, API: APIProvider<Ctx = Mutex<Lua>>> ScriptHost for RLuaScriptHost<A, API> {
    type ScriptContext = Mutex<Lua>;
    type ScriptEvent = LuaEvent<A>;
    type ScriptAsset = LuaFile;

    fn register_with_app(app: &mut App, stage: impl StageLabel) {
        app.add_priority_event::<Self::ScriptEvent>();
        app.add_asset::<LuaFile>();
        app.init_asset_loader::<LuaLoader>();
        app.init_resource::<CachedScriptEventState<Self::ScriptEvent>>();
        app.init_resource::<ScriptContexts<Self>>();

        app.add_system_set_to_stage(
            stage,
            SystemSet::new()
                .with_system(script_add_synchronizer::<Self>)
                .with_system(script_remove_synchronizer::<Self>)
                .with_system(script_hot_reload_handler::<Self>),
        );
    }

    fn load_script(script: &[u8], script_name: &str) -> Result<Self::ScriptContext> {
        let lua = Lua::new();
        lua.context::<_, Result<()>>(|lua_ctx| {
            lua_ctx
                .load(script)
                .set_name(script_name)
                .map(|c| c.exec())
                .map_err(|_e| anyhow!("Error loading script {}", script_name))??;

            Ok(())
        })?;

        let mut lua = Mutex::new(lua);

        API::attach_api(&mut lua);

        Ok(lua)
    }

    fn handle_events(world: &mut World, events: &[Self::ScriptEvent]) -> Result<()> {
        // we need to do this since scripts need access to the world, but they also
        // live in it, hence we only store indices into a resource which can then be scoped
        // instead of storing contexts directly on the components
        world.resource_scope(|world, res: Mut<ScriptContexts<Self>>| {
            res.context_entities
                .values()
                .filter_map(|(entity, ctx)| ctx.as_ref().map(|v| (entity, v)))
                .map(|(entity, ctx)| {
                    let world_ptr = LuaLightUserData(world as *mut World as *mut c_void);
                    let lua_ctx = ctx.lock().unwrap();

                    lua_ctx.context::<_, Result<()>>(|lua_ctx| {
                        let globals = lua_ctx.globals();
                        globals.set("world", world_ptr)?;
                        globals.set("entity", entity.to_bits())?;

                        // event order is preserved, but scripts can't rely on any temporal
                        // guarantees when it comes to other scripts callbacks,
                        // at least for now
                        for event in events.iter() {
                            let mut f: Function = match globals.get(event.hook_name.clone()) {
                                Ok(f) => f,
                                Err(_) => continue, // not subscribed to this event
                            };

                            // bind arguments and catch any errors
                            f = event.args.clone().into_iter().fold(Ok(f), |a, i| match a {
                                Ok(f) => f.bind(i.to_lua(lua_ctx)),
                                Err(e) => Err(e),
                            })?;

                            f.call::<MultiValue, ()>(event.args.clone().to_lua_multi(lua_ctx)?)
                                .map_err(|e| anyhow!("Runtime LUA error: {}", e))?;
                        }

                        Ok(())
                    })
                })
                .bcollect()
        })
    }
}
impl<A: LuaArg, API: APIProvider<Ctx = Mutex<Lua>>> RLuaScriptHost<A, API> {
    pub fn register_api_callback<F, Arg, R>(
        callback_fn_name: &str,
        callback: F,
        script: &<Self as ScriptHost>::ScriptContext,
    ) where
        Arg: for<'lua> FromLuaMulti<'lua>,
        R: for<'lua> ToLuaMulti<'lua>,
        F: 'static + Send + for<'lua> Fn(Context<'lua>, Arg) -> Result<R, LuaError>,
    {
        script.lock().unwrap().context(|lua_ctx| {
            let f = lua_ctx.create_function(callback).unwrap();
            lua_ctx.globals().set(callback_fn_name, f).unwrap();
        });
    }
}
