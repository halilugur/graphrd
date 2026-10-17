package com.example.personnel.model.entity;

import org.graphrd.rdf.RdfId;
import org.graphrd.rdf.RdfProperty;
import org.graphrd.rdf.RdfType;

import lombok.AllArgsConstructor;
import lombok.Getter;
import lombok.NoArgsConstructor;

/** A company in the graph; employees point at it by IRI. */
@Getter
@NoArgsConstructor
@AllArgsConstructor
@RdfType("Company")
public class Company {

    @RdfId
    String id;

    @RdfProperty("name")
    String name;

    @RdfProperty("industry")
    String industry;
}
