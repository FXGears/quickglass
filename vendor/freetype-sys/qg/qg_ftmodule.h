/*
 * QuickGlass module list for the bundled (Windows) FreeType build.
 *
 * The Windows shell only ever loads TrueType faces (Segoe UI and Consolas,
 * .ttf), so only the TrueType driver, the SFNT container, the smooth (AA)
 * renderer, PostScript glyph names and the auto-hinter are compiled in.
 * Selected via FT_CONFIG_MODULES_H in build.rs.
 */
FT_USE_MODULE( FT_Module_Class, autofit_module_class )
FT_USE_MODULE( FT_Driver_ClassRec, tt_driver_class )
FT_USE_MODULE( FT_Module_Class, psnames_module_class )
FT_USE_MODULE( FT_Module_Class, sfnt_module_class )
FT_USE_MODULE( FT_Renderer_Class, ft_smooth_renderer_class )
