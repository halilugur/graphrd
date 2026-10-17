package com.example.personnel;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.boot.context.properties.ConfigurationPropertiesScan;
import org.springframework.boot.context.properties.ConfigurationPropertiesScan;
import org.springframework.data.web.config.EnableSpringDataWebSupport;


/**
 * A personnel application that keeps everything in GraphRD through the client
 * library: entities in the graph, joins by IRI, reports computed by the
 * server in SPARQL.
 *
 * Start GraphRD first ({@code graphrd serve}), then {@code mvn spring-boot:run}.
 */
@SpringBootApplication
@ConfigurationPropertiesScan
@EnableSpringDataWebSupport(pageSerializationMode = EnableSpringDataWebSupport.PageSerializationMode.VIA_DTO)   // ?page&size&sort resolve to Pageable; Page answers as a stable DTO
public class PersonnelApplication {

    public static void main(String[] args) {
        SpringApplication.run(PersonnelApplication.class, args);
    }
}
